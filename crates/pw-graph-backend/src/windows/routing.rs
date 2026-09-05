//! Links qpwgraph owns on Windows, and the audio that makes them real.
//!
//! Core Audio reports which endpoint a session is attached to. It does not
//! offer a supported way to move one, which is why the observed session links
//! stay immutable. What Windows *does* allow from user mode is opening any
//! endpoint for capture, any playback endpoint for loopback, and any playback
//! endpoint for render — and once qpwgraph holds both ends it can carry the
//! audio between them itself.
//!
//! So a link drawn between two endpoint ports here is not a projection of
//! something Windows already decided. It is a route in [`crate::router`],
//! with real WASAPI streams at both ends, and disconnecting it stops real
//! audio. That is the whole difference between this module and the observed
//! graph next door.
//!
//! # What is routable
//!
//! | From | To | How |
//! | --- | --- | --- |
//! | a recording endpoint | a playback endpoint | capture → render |
//! | a playback endpoint's monitor | a playback endpoint | loopback → render |
//! | a virtualized application session | anything | process loopback → router |
//!
//! An ordinary application session is still refused: Windows does not expose
//! a supported way to re-point it, and capturing it without first moving the
//! stream to QPWGraph Virtual Output would produce dry + processed duplicate
//! audio. A session already attached to that virtual endpoint is isolated and
//! can use process-loopback capture on supported Windows builds.
//!
//! # Ownership
//!
//! The route table is always rebuilt from the link set and installed in one
//! transaction, so a rejected change leaves the working routes running. A
//! device is opened when the first link needs it and closed when the last one
//! stops, and the closing happens on the caller's thread rather than between
//! two blocks of audio.

use super::app_route_policy::verify_live_process_identity;
use super::*;

use crate::router::engine::{
    BranchSpec, DestinationSpec, ProcessorId, RouteId, RouteSpec, RouterConfig, RouterError,
    SinkId, SourceId,
};
use crate::router::format::AudioFormat;
use crate::router::thread::{RouterStopped, RouterThread};
use crate::router::wasapi::{self, WasapiEndpoint};
use crate::router::{MeterReading, RouteMetrics};
use pw_graph_effects::{AudioSpec, EffectProcessor};

/// The format every qpwgraph-owned Windows route runs at.
///
/// WASAPI is asked to convert to it on both ends, so an endpoint at 44.1 kHz
/// or 7.1 still meets the router here and the router's own resampler is left
/// to handle drift rather than device geometry.
const ROUTE_FORMAT: AudioFormat = AudioFormat::new(48_000, 2);

/// Frames buffered between a device thread and the router, per stream.
///
/// 4096 frames is about 85 ms at 48 kHz: enough to absorb a scheduling
/// hiccup on either side, and bounded, so a stalled consumer loses audio and
/// increments a counter instead of growing latency without end.
const RING_FRAMES: usize = 4_096;

/// Frames per router cycle: 10 ms at 48 kHz, inside a WASAPI shared-mode
/// period and coarse enough that per-block overhead stays negligible.
const BLOCK_FRAMES: usize = 480;

/// One WASAPI stream, kept alive for as long as some link needs it.
struct Device {
    endpoint: DeviceEndpoint,
    /// Links currently using this device. The stream closes when it reaches
    /// zero, so unplugging the last route also releases the hardware.
    users: usize,
}

enum DeviceEndpoint {
    Wasapi(WasapiEndpoint),
    Process(ProcessLoopbackSource),
}

impl DeviceEndpoint {
    fn stop(&mut self) {
        match self {
            Self::Wasapi(endpoint) => endpoint.stop(),
            Self::Process(endpoint) => endpoint.stop(),
        }
    }
}

/// An effect node's two ports and the processor between them.
///
/// Keyed by input port in [`WindowsRouting::effects`], because that is the
/// end a link arrives at: walking a route means "this link ends at an effect
/// input, so continue from its output".
struct Effect {
    processor: ProcessorId,
    output_port: PortId,
}

/// How many effects one path may chain before the walk gives up.
///
/// A guard against a cycle in the graph, not a product limit: sixteen effects
/// in series is already far past useful, and recursing forever is not a
/// failure mode a patchbay file should be able to cause.
const MAX_CHAIN: usize = 16;

/// The audio behind qpwgraph's own Windows links.
pub(super) struct WindowsRouting {
    router: RouterThread,
    /// Links this backend created, by id. The route table is derived from
    /// this map, never edited in place.
    links: BTreeMap<LinkId, Link>,
    sources: BTreeMap<PortId, (SourceId, Device)>,
    sinks: BTreeMap<PortId, (SinkId, Device)>,
    /// Effect nodes, by input port.
    effects: BTreeMap<PortId, Effect>,
    /// Software gain per routed source port, applied to everything it feeds.
    source_gains: BTreeMap<PortId, f32>,
    /// Each effect's output port, so a link leaving one is recognised as
    /// routable without searching every effect.
    effect_outputs: BTreeMap<PortId, PortId>,
    next_id: u64,
}

impl std::fmt::Debug for WindowsRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsRouting")
            .field("links", &self.links.len())
            .field("sources", &self.sources.len())
            .field("sinks", &self.sinks.len())
            .finish()
    }
}

impl WindowsRouting {
    pub(super) fn start() -> BackendResult<Self> {
        let router = RouterThread::start(RouterConfig {
            block_frames: BLOCK_FRAMES,
            clock_rate: ROUTE_FORMAT.sample_rate,
        })
        .map_err(|error| {
            BackendError::native(format!("could not start the Windows audio router: {error}"))
        })?;
        Ok(Self {
            router,
            links: BTreeMap::new(),
            sources: BTreeMap::new(),
            sinks: BTreeMap::new(),
            effects: BTreeMap::new(),
            effect_outputs: BTreeMap::new(),
            source_gains: BTreeMap::new(),
            next_id: 1,
        })
    }

    pub(super) fn links(&self) -> impl Iterator<Item = &Link> {
        self.links.values()
    }

    pub(super) fn owns(&self, link: LinkId) -> bool {
        self.links.contains_key(&link)
    }

    /// What each route has actually been doing: frames carried, dropouts,
    /// conversion ratio, and the last fault.
    ///
    /// Reading these never touches the audio path — they are atomics — which
    /// is what makes "is this link carrying anything?" a question the UI can
    /// ask per frame.
    pub(super) fn metrics(&self) -> Vec<(LinkId, RouteMetrics)> {
        let mut out = Vec::with_capacity(self.links.len());
        for link in self.links.values() {
            // Routes are keyed by output port, so every link sharing a source
            // reports that route's counters.
            let route = RouteId(link.output_port.0);
            let Ok(Some(metrics)) = self.router.with(move |core| core.metrics(route)) else {
                continue;
            };
            out.push((link.id, metrics));
        }
        out
    }

    /// The level at each routed source port, with a real RMS.
    ///
    /// Keyed by the port the audio leaves, so a caller can attach the reading
    /// to the node the user is looking at. Only routed ports appear: a device
    /// qpwgraph is not carrying has no PCM here to measure, and Core Audio's
    /// own peak meter remains the honest answer for it.
    pub(super) fn port_meters(&self) -> Vec<(PortId, MeterReading)> {
        let mut out = Vec::with_capacity(self.sources.len());
        for port in self.sources.keys() {
            let route = RouteId(port.0);
            let Ok(Some(reading)) = self.router.with(move |core| core.source_meter(route)) else {
                continue;
            };
            if reading.available {
                out.push((*port, reading));
            }
        }
        out
    }

    /// Whether this port is the source of a route qpwgraph is carrying.
    ///
    /// What makes software gain above unity and a real RMS available on the
    /// node that owns it.
    pub(super) fn carries_source(&self, port: PortId) -> bool {
        self.sources.contains_key(&port)
    }

    /// Gain currently applied to everything a source feeds.
    pub(super) fn source_gain(&self, port: PortId) -> f32 {
        self.source_gains.get(&port).copied().unwrap_or(1.0)
    }

    /// Apply software gain to everything a source feeds.
    ///
    /// This is the boost §13 of the parity roadmap asks for. A Windows
    /// endpoint's own volume control stops at unity and is reported honestly;
    /// anything past that is this, applied to audio qpwgraph owns, and it
    /// exists only while the route does.
    pub(super) fn set_source_gain(&mut self, port: PortId, gain: f32) -> BackendResult<()> {
        if !self.sources.contains_key(&port) {
            return Err(BackendError::unsupported(
                "software gain applies only to a source qpwgraph is routing",
            ));
        }
        if self.source_gain(port) == gain {
            return Ok(());
        }
        let previous = self.source_gains.insert(port, gain);
        match self.install() {
            Ok(()) => Ok(()),
            Err(error) => {
                match previous {
                    Some(previous) => {
                        self.source_gains.insert(port, previous);
                    }
                    None => {
                        self.source_gains.remove(&port);
                    }
                }
                Err(error)
            }
        }
    }

    /// The geometry every effect on a Windows route is prepared for.
    ///
    /// Routes run at a fixed format, so an effect can be prepared once by the
    /// caller and the router never has to hand a processor a buffer shape it
    /// did not agree to.
    pub(super) fn effect_spec(block_frames: u32) -> AudioSpec {
        AudioSpec {
            sample_rate: ROUTE_FORMAT.sample_rate,
            channels: ROUTE_FORMAT.channels,
            max_frames: block_frames,
        }
    }

    /// The block size routes run at, which every effect must accept.
    pub(super) fn block_frames() -> u32 {
        BLOCK_FRAMES as u32
    }

    /// Register an effect node's processor and its two ports.
    ///
    /// The node is inert until something links into it: an effect with no
    /// audio reaching its input simply is not on any route, which is exactly
    /// what a free-standing effect node on the canvas should be.
    pub(super) fn add_effect(
        &mut self,
        input_port: PortId,
        output_port: PortId,
        processor: Box<dyn EffectProcessor>,
        spec: AudioSpec,
    ) -> BackendResult<()> {
        if self.effects.contains_key(&input_port) {
            return Err(BackendError::native("that effect is already registered"));
        }
        let id = ProcessorId(self.take_id());
        self.router
            .with(move |core| core.add_processor(id, processor, spec))
            .map_err(router_stopped)?
            .map_err(router_error)?;
        self.effects.insert(
            input_port,
            Effect {
                processor: id,
                output_port,
            },
        );
        self.effect_outputs.insert(output_port, input_port);
        Ok(())
    }

    /// Remove an effect node, along with any links that touched it.
    ///
    /// The links go first and the table is reinstalled before the processor
    /// is taken back, so the router has already stopped calling it. Returns
    /// the links that were dropped, for the caller to remove from the graph.
    pub(super) fn remove_effect(&mut self, input_port: PortId) -> BackendResult<Vec<Link>> {
        let Some(effect) = self.effects.remove(&input_port) else {
            return Err(BackendError::native("no such effect is registered"));
        };
        let output_port = effect.output_port;
        self.effect_outputs.remove(&output_port);
        let dropped: Vec<Link> = self
            .links
            .values()
            .filter(|link| link.output_port == output_port || link.input_port == input_port)
            .cloned()
            .collect();
        for link in &dropped {
            self.links.remove(&link.id);
        }
        if let Err(error) = self.install() {
            // `install` is all-or-nothing in the router core. Restore the
            // control-plane tables as well, so a rejected removal does not
            // leave the effect apparently gone while the old route remains
            // live.
            self.effects.insert(input_port, effect);
            self.effect_outputs.insert(output_port, input_port);
            for link in &dropped {
                self.links.insert(link.id, link.clone());
            }
            return Err(error);
        }
        for link in &dropped {
            self.release_source(link.output_port);
            self.release_sink(link.input_port);
        }
        let processor = effect.processor;
        // Handed back here so the processor is dropped on this thread rather
        // than between two blocks of audio.
        let released = self
            .router
            .with(move |core| core.remove_processor(processor));
        drop(released);
        Ok(dropped)
    }

    /// Bypass or re-enable an effect without disturbing its configuration or
    /// its place in the chain.
    pub(super) fn set_effect_bypassed(
        &mut self,
        input_port: PortId,
        bypassed: bool,
    ) -> BackendResult<()> {
        let processor = self.processor_at(input_port)?;
        self.router
            .with(move |core| core.set_processor_bypassed(processor, bypassed))
            .map_err(router_stopped)?
            .map_err(router_error)
    }

    pub(super) fn set_effect_parameter(
        &mut self,
        input_port: PortId,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        let processor = self.processor_at(input_port)?;
        let parameter = parameter.to_owned();
        self.router
            .with(move |core| core.set_processor_parameter(processor, &parameter, value))
            .map_err(router_stopped)?
            .map_err(router_error)
    }

    fn processor_at(&self, input_port: PortId) -> BackendResult<ProcessorId> {
        self.effects
            .get(&input_port)
            .map(|effect| effect.processor)
            .ok_or_else(|| BackendError::native("no such effect is registered"))
    }

    /// Carry audio from `output` to `input` for real.
    ///
    /// Both ports must be endpoint ports the router can open; a session port
    /// is refused here rather than accepted and quietly ignored. The devices
    /// are opened first, then the whole route table is reinstalled in one
    /// transaction, so a failure at either step leaves the previous routes
    /// exactly as they were.
    pub(super) fn connect(
        &mut self,
        link: Link,
        endpoint_ports: &BTreeMap<PortId, EndpointPort>,
    ) -> BackendResult<()> {
        if self.links.contains_key(&link.id) {
            return Err(BackendError::native("that route already exists"));
        }
        // An effect node's ports are routable without being a device: a link
        // into one continues a chain rather than ending it.
        let output = if self.effect_outputs.contains_key(&link.output_port) {
            None
        } else {
            Some(routable(endpoint_ports, link.output_port, PortEnd::Output)?)
        };
        let input = if self.effects.contains_key(&link.input_port) {
            None
        } else {
            Some(routable(endpoint_ports, link.input_port, PortEnd::Input)?)
        };

        if let Some(output) = output {
            self.ensure_source(link.output_port, output)?;
        }
        if let Some(input) = input {
            if let Err(error) = self.ensure_sink(link.input_port, input) {
                // Undo the half of the pair that did open, so a failed connect
                // leaves no device held open by nothing.
                self.release_source(link.output_port);
                return Err(error);
            }
        }

        let (id, output_port, input_port) = (link.id, link.output_port, link.input_port);
        self.links.insert(id, link);
        match self.install() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.links.remove(&id);
                self.release_source(output_port);
                self.release_sink(input_port);
                // The previous table is still installed, because `set_routes`
                // is all-or-nothing; there is nothing to restore.
                Err(error)
            }
        }
    }

    /// Stop carrying a route, releasing any device it was the last user of.
    pub(super) fn disconnect(&mut self, id: LinkId) -> BackendResult<Link> {
        let link = self
            .links
            .remove(&id)
            .ok_or_else(|| BackendError::native("that route is not one this backend created"))?;
        // Reinstalling first means the router has already let go of the
        // devices by the time they are closed.
        if let Err(error) = self.install() {
            // The router kept the old table after rejecting the replacement;
            // put the link back before returning so device ownership and the
            // control-plane graph still describe that live table.
            self.links.insert(link.id, link.clone());
            return Err(error);
        }
        self.release_source(link.output_port);
        self.release_sink(link.input_port);
        Ok(link)
    }

    /// Drop every route whose ports no longer exist.
    ///
    /// Called after a graph rebuild: an unplugged device takes its ports with
    /// it, and a route pointing at a port that is gone is not a route. The
    /// links that survive are returned to the caller to re-add to the graph.
    pub(super) fn reconcile(&mut self, live_ports: &BTreeSet<PortId>) -> BackendResult<()> {
        let stale: Vec<Link> = self
            .links
            .values()
            .filter(|link| {
                !live_ports.contains(&link.output_port) || !live_ports.contains(&link.input_port)
            })
            .cloned()
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        for link in &stale {
            self.links.remove(&link.id);
        }
        if let Err(error) = self.install() {
            for link in &stale {
                self.links.insert(link.id, link.clone());
            }
            return Err(error);
        }
        for link in &stale {
            self.release_source(link.output_port);
            self.release_sink(link.input_port);
        }
        Ok(())
    }

    /// Reopen every WASAPI worker used by a route that reported a lost
    /// endpoint.
    ///
    /// The paced router deliberately reports device loss instead of trying to
    /// open a replacement from its audio thread.  This method is the control
    /// plane half of that boundary: it removes the old sources and sinks only
    /// after the route table is empty, opens the current endpoint identities,
    /// and installs the same link-derived table again.  Links, effects, gain,
    /// and route ids stay intact throughout the recovery attempt.
    pub(super) fn recover_lost(
        &mut self,
        endpoint_ports: &BTreeMap<PortId, EndpointPort>,
    ) -> BackendResult<()> {
        let lost = self.router.take_lost_routes();
        if lost.is_empty() {
            return Ok(());
        }

        // A route id is derived from its source port.  A source may fan out to
        // several links, so recovering one id is enough to rebuild the shared
        // device and route table; retain only ids that still have a live link
        // after graph reconciliation.
        let recovered: Vec<RouteId> = lost
            .into_iter()
            .filter(|route| {
                self.links
                    .values()
                    .any(|link| RouteId(link.output_port.0) == *route)
            })
            .collect();
        if recovered.is_empty() {
            return Ok(());
        }

        // Recreate one worker per endpoint port, counting every surviving link
        // that shares it.  Effect ports are software-only and therefore do not
        // appear in endpoint_ports; the adjacent endpoint link supplies the
        // real source or sink.
        let links: Vec<Link> = self.links.values().cloned().collect();
        // Validate the complete set before tearing down the working devices.
        // A graph refresh can race endpoint removal; returning an explained
        // error while the old route is still alive is safer than leaving a
        // half-reopened route with no installed table.
        let validate = || {
            for link in &links {
                if !self.effect_outputs.contains_key(&link.output_port) {
                    routable(endpoint_ports, link.output_port, PortEnd::Output)?;
                }
                if !self.effects.contains_key(&link.input_port) {
                    routable(endpoint_ports, link.input_port, PortEnd::Input)?;
                }
            }
            Ok::<(), BackendError>(())
        };
        if let Err(error) = validate() {
            self.router.requeue_lost_routes(&recovered);
            return Err(error);
        }

        if let Err(error) = self.clear_devices() {
            self.router.requeue_lost_routes(&recovered);
            return Err(error);
        }

        for link in &links {
            if !self.effect_outputs.contains_key(&link.output_port) {
                let endpoint = match routable(endpoint_ports, link.output_port, PortEnd::Output) {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        let _ = self.clear_devices();
                        self.router.requeue_lost_routes(&recovered);
                        return Err(error);
                    }
                };
                if let Err(error) = self.ensure_source(link.output_port, endpoint) {
                    let _ = self.clear_devices();
                    self.router.requeue_lost_routes(&recovered);
                    return Err(error);
                }
            }
            if !self.effects.contains_key(&link.input_port) {
                let endpoint = match routable(endpoint_ports, link.input_port, PortEnd::Input) {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        let _ = self.clear_devices();
                        self.router.requeue_lost_routes(&recovered);
                        return Err(error);
                    }
                };
                if let Err(error) = self.ensure_sink(link.input_port, endpoint) {
                    let _ = self.clear_devices();
                    self.router.requeue_lost_routes(&recovered);
                    return Err(error);
                }
            }
        }

        if let Err(error) = self.install() {
            let _ = self.clear_devices();
            self.router.requeue_lost_routes(&recovered);
            return Err(error);
        }

        // Every owned worker was replaced above, not just the one that first
        // reported loss. A shared destination or source can carry another
        // route, and its resampler/effect state is just as stale after the
        // worker restart. Reset every installed route so no old PCM crosses
        // the discontinuity.
        let reset_routes = match self.router.with(|core| core.route_ids()) {
            Ok(routes) => routes,
            Err(error) => {
                self.router.requeue_lost_routes(&recovered);
                return Err(router_stopped(error));
            }
        };
        let reset = self.router.with(move |core| {
            for route in reset_routes {
                core.reset_route(route)?;
            }
            Ok::<(), RouterError>(())
        });
        match reset {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.router.requeue_lost_routes(&recovered);
                Err(router_error(error))
            }
            Err(error) => {
                self.router.requeue_lost_routes(&recovered);
                Err(router_stopped(error))
            }
        }
    }

    /// Remove all device workers after the router has stopped using them.
    ///
    /// The returned audio objects are dropped on this control thread, not on
    /// the paced router thread, so a COM/WASAPI teardown cannot happen in the
    /// middle of a block.
    fn clear_devices(&mut self) -> BackendResult<()> {
        let source_entries = std::mem::take(&mut self.sources);
        let sink_entries = std::mem::take(&mut self.sinks);
        let source_ids: Vec<SourceId> = source_entries.values().map(|(id, _)| *id).collect();
        let sink_ids: Vec<SinkId> = sink_entries.values().map(|(id, _)| *id).collect();

        let released = self.router.with(move |core| {
            core.set_routes(&[])?;
            let mut sources = Vec::with_capacity(source_ids.len());
            for id in source_ids {
                sources.push(core.remove_source(id)?);
            }
            let mut sinks = Vec::with_capacity(sink_ids.len());
            for id in sink_ids {
                sinks.push(core.remove_sink(id)?);
            }
            Ok::<_, RouterError>((sources, sinks))
        });

        match released {
            Ok(Ok((sources, sinks))) => {
                // Keep the explicit drops here: these boxes own the ring
                // handles, and their destruction belongs outside the router
                // thread's closure.
                drop(sources);
                drop(sinks);
                for (_, (_, mut device)) in source_entries {
                    device.endpoint.stop();
                }
                for (_, (_, mut device)) in sink_entries {
                    device.endpoint.stop();
                }
                Ok(())
            }
            Ok(Err(error)) => {
                // This should only be reachable for an internal invariant
                // violation (the table was emptied first), but do not leak a
                // worker if the router reports one.
                for (_, (_, mut device)) in source_entries {
                    device.endpoint.stop();
                }
                for (_, (_, mut device)) in sink_entries {
                    device.endpoint.stop();
                }
                Err(router_error(error))
            }
            Err(error) => {
                for (_, (_, mut device)) in source_entries {
                    device.endpoint.stop();
                }
                for (_, (_, mut device)) in sink_entries {
                    device.endpoint.stop();
                }
                Err(router_stopped(error))
            }
        }
    }

    /// Rebuild the route table from the current link set and install it.
    ///
    /// This is where the drawn graph becomes a route table. Starting at each
    /// device source, the links are walked forward: a link landing on a
    /// playback device is a destination, and a link landing on an effect
    /// node's input continues from that node's output with the effect added
    /// to the chain.
    ///
    /// Paths that end up with the same chain of effects become one branch, so
    /// a source feeding two speakers through the same effect runs it once.
    /// Paths with different chains become different branches, which is what
    /// stops an effect inserted into one link from processing a sibling
    /// fan-out. Either way the source is pulled exactly once per block.
    fn install(&mut self) -> BackendResult<()> {
        let mut forward: BTreeMap<PortId, Vec<PortId>> = BTreeMap::new();
        for link in self.links.values() {
            forward
                .entry(link.output_port)
                .or_default()
                .push(link.input_port);
        }

        let mut specs = Vec::with_capacity(self.sources.len());
        for (output, (source, _)) in &self.sources {
            let mut chains: BTreeMap<Vec<ProcessorId>, Vec<DestinationSpec>> = BTreeMap::new();
            walk(
                &forward,
                &self.sinks,
                &self.effects,
                *output,
                &mut Vec::new(),
                &mut chains,
            );
            if chains.is_empty() {
                // The source is registered but nothing downstream resolves to
                // a device yet -- a half-drawn chain, say. Not a route.
                continue;
            }
            specs.push(RouteSpec {
                // Derived from the output port, so a route keeps its meters
                // and counters when a destination is added or removed.
                id: RouteId(output.0),
                source: *source,
                gain: self.source_gains.get(output).copied().unwrap_or(1.0),
                branches: chains
                    .into_iter()
                    .map(|(processors, destinations)| BranchSpec {
                        processors,
                        gain: 1.0,
                        destinations,
                    })
                    .collect(),
            });
        }

        self.router
            .with(move |core| core.set_routes(&specs))
            .map_err(router_stopped)?
            .map_err(router_error)
    }

    /// Take a use of the device behind an output port, opening it if this is
    /// the first route that needs it.
    fn ensure_source(&mut self, port: PortId, endpoint: &EndpointPort) -> BackendResult<()> {
        if let Some((_, device)) = self.sources.get_mut(&port) {
            device.users += 1;
            return Ok(());
        }
        let device_id = Some(endpoint.device_id.as_str());
        let (source, endpoint_worker) = match &endpoint.role {
            EndpointPortRole::Capture => {
                let (source, worker) =
                    wasapi::open_capture_source(device_id, ROUTE_FORMAT, RING_FRAMES)?;
                (source, DeviceEndpoint::Wasapi(worker))
            }
            EndpointPortRole::Monitor => {
                let (source, worker) =
                    wasapi::open_loopback_source(device_id, ROUTE_FORMAT, RING_FRAMES)?;
                (source, DeviceEndpoint::Wasapi(worker))
            }
            EndpointPortRole::Process { pid, selector } => {
                verify_live_process_identity(selector, *pid)?;
                let (source, worker) = ProcessLoopbackSource::open(
                    *pid,
                    ProcessLoopbackMode::IncludeProcessTree,
                    ROUTE_FORMAT,
                    RING_FRAMES,
                )?;
                (source, DeviceEndpoint::Process(worker))
            }
            EndpointPortRole::Render => {
                return Err(BackendError::unsupported(
                    "a playback device is not a source; drag from its monitor instead",
                ))
            }
        };
        let id = SourceId(self.take_id());
        self.router
            .with(move |core| core.add_source(id, Box::new(source)))
            .map_err(router_stopped)?
            .map_err(router_error)?;
        self.sources.insert(
            port,
            (
                id,
                Device {
                    endpoint: endpoint_worker,
                    users: 1,
                },
            ),
        );
        Ok(())
    }

    fn ensure_sink(&mut self, port: PortId, endpoint: &EndpointPort) -> BackendResult<()> {
        if let Some((_, device)) = self.sinks.get_mut(&port) {
            device.users += 1;
            return Ok(());
        }
        if endpoint.role != EndpointPortRole::Render {
            return Err(BackendError::unsupported(
                "only a playback device can be the destination of a Windows audio route",
            ));
        }
        let (sink, wasapi) =
            wasapi::open_render_sink(Some(endpoint.device_id.as_str()), ROUTE_FORMAT, RING_FRAMES)?;
        let id = SinkId(self.take_id());
        self.router
            .with(move |core| core.add_sink(id, Box::new(sink)))
            .map_err(router_stopped)?
            .map_err(router_error)?;
        self.sinks.insert(
            port,
            (
                id,
                Device {
                    endpoint: DeviceEndpoint::Wasapi(wasapi),
                    users: 1,
                },
            ),
        );
        Ok(())
    }

    /// Give up one use of an output port's device, closing it if that was the
    /// last one.
    fn release_source(&mut self, port: PortId) {
        let Some((id, device)) = self.sources.get_mut(&port) else {
            return;
        };
        device.users -= 1;
        if device.users > 0 {
            return;
        }
        let id = *id;
        // The router hands the source back so it is dropped here rather than
        // between two blocks of audio, and the WASAPI thread is joined on
        // this thread for the same reason.
        let released = self.router.with(move |core| core.remove_source(id));
        drop(released);
        if let Some((_, mut device)) = self.sources.remove(&port) {
            device.endpoint.stop();
        }
    }

    fn release_sink(&mut self, port: PortId) {
        let Some((id, device)) = self.sinks.get_mut(&port) else {
            return;
        };
        device.users -= 1;
        if device.users > 0 {
            return;
        }
        let id = *id;
        let released = self.router.with(move |core| core.remove_sink(id));
        drop(released);
        if let Some((_, mut device)) = self.sinks.remove(&port) {
            device.endpoint.stop();
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Follow every link out of `port`, collecting the destinations each distinct
/// chain of effects reaches.
///
/// Recursive because the graph is: an effect's output is just another port
/// with links of its own. `chain` is the effects passed through to get here,
/// and it doubles as the cycle guard — an effect already in the chain would
/// otherwise be re-entered forever.
fn walk(
    forward: &BTreeMap<PortId, Vec<PortId>>,
    sinks: &BTreeMap<PortId, (SinkId, Device)>,
    effects: &BTreeMap<PortId, Effect>,
    port: PortId,
    chain: &mut Vec<ProcessorId>,
    chains: &mut BTreeMap<Vec<ProcessorId>, Vec<DestinationSpec>>,
) {
    let Some(next) = forward.get(&port) else {
        return;
    };
    for &input in next {
        if let Some((sink, _)) = sinks.get(&input) {
            let destinations = chains.entry(chain.clone()).or_default();
            if !destinations
                .iter()
                .any(|destination| destination.sink == *sink)
            {
                destinations.push(DestinationSpec::new(*sink));
            }
            continue;
        }
        let Some(effect) = effects.get(&input) else {
            continue;
        };
        if chain.len() >= MAX_CHAIN || chain.contains(&effect.processor) {
            // A cycle, or a chain long past anything useful. Dropping the
            // path is better than recursing until the stack runs out.
            continue;
        }
        chain.push(effect.processor);
        walk(forward, sinks, effects, effect.output_port, chain, chains);
        chain.pop();
    }
}

enum PortEnd {
    Output,
    Input,
}

/// Resolve a port to the device behind it, or explain why it has none.
fn routable(
    endpoint_ports: &BTreeMap<PortId, EndpointPort>,
    port: PortId,
    end: PortEnd,
) -> BackendResult<&EndpointPort> {
    endpoint_ports.get(&port).ok_or_else(|| {
        // Almost always an application session. Say what Windows cannot do
        // rather than reporting a missing port the user can plainly see.
        BackendError::unsupported(match end {
            PortEnd::Output => {
                "only a recording device, a playback device's monitor, or an application already \
                 attached to QPWGraph Virtual Output can be the source of a Windows audio route; \
                 move the application in Windows Volume Mixer before enabling process loopback"
            }
            PortEnd::Input => {
                "only a playback device can be the destination of a Windows audio route; Windows \
                 exposes no supported way to re-point an application's stream"
            }
        })
    })
}

fn router_error(error: RouterError) -> BackendError {
    BackendError::native(format!("Windows audio route failed: {error}"))
}

fn router_stopped(_: RouterStopped) -> BackendError {
    BackendError::native("the Windows audio router is not running")
}

/// A stable link id for a pair of ports, so a route keeps its identity across
/// a graph rebuild.
pub(super) fn managed_link(output: PortId, input: PortId) -> Link {
    Link {
        id: LinkId(graph_id(managed_link_local_id(output, input))),
        output_port: output,
        input_port: input,
    }
}
