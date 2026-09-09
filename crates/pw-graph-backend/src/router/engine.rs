//! The routing engine: sources, sinks, effects, and the block cycle that
//! moves audio between them.
//!
//! This is the part of the Windows parity work that has to exist before
//! `connect` can ever return `Ok`. Core Audio lets qpwgraph *observe* which
//! endpoint a session is attached to; it does not let qpwgraph move one. A
//! link the user can actually drag requires qpwgraph to own the PCM, and
//! owning the PCM means owning a router. Everything here is deliberately
//! platform-neutral: it is driven by [`AudioSource`] and [`AudioSink`], so the
//! same engine is exercised by in-memory tests on any host and by WASAPI
//! endpoints on Windows.
//!
//! # The cycle
//!
//! One call to [`RouterCore::process`] moves at most one block:
//!
//! 1. every route pulls a block from its source;
//! 2. the route's gain is applied, then its effect chain, in place;
//! 3. the route's meter observes the result (**post-effect**, see
//!    [`super::meter`]);
//! 4. for each destination the block is channel-mapped, rate-converted, and
//!    *added* into that sink's mix accumulator;
//! 5. every sink is written exactly once.
//!
//! Step 4 is what gives fan-out and mixing at the same time: one source
//! reaching several sinks is fan-out, several sources reaching one sink is a
//! mix, and neither needs a special case.
//!
//! # Threading
//!
//! [`RouterCore`] is single-threaded on purpose. Devices never call into it;
//! they hand audio over through the bounded ring in [`super::buffer`], and the
//! router pulls on its own thread. So structural changes -- adding a source,
//! replacing the route table -- run between blocks on that thread rather than
//! inside a device callback, and the prohibitions in §8.1 of the parity
//! roadmap apply where they matter: [`RouterCore::process`] itself allocates
//! nothing, locks nothing, and formats no strings.
//!
//! # Transactions
//!
//! [`RouterCore::set_routes`] validates and builds every route before it
//! touches the live table. A route table that cannot be built in full is not
//! installed in part: a half-applied reroute is the "partially torn-down audio
//! route" §9.3 calls a bug.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use pw_graph_effects::{AudioSpec, EffectError, EffectProcessor};

use super::diagnostics::{RouteDiagnostics, RouteFault, RouteMetrics};
use super::format::{AudioFormat, ChannelMap};
use super::meter::{MeterCell, MeterReading};
use super::resample::Resampler;

/// Identifies a source registered with the router.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceId(pub u64);

/// Identifies a sink registered with the router.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SinkId(pub u64);

/// Identifies an effect instance registered with the router.
///
/// Stable across route changes, which is what §12.1 means by "persistent
/// instance identity": moving an effect between links must not reset its
/// parameters.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProcessorId(pub u64);

/// Identifies a route. Maps to one qpwgraph link, or to the one link a chain
/// of effects was inserted into.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RouteId(pub u64);

/// How healthy a device is, as its adapter sees it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StreamHealth {
    /// Audio is flowing.
    #[default]
    Ok,
    /// Nothing available this block, but the device is fine. A capture ring
    /// that has not filled yet is here, not in `Lost`.
    Starved,
    /// The device is gone: unplugged, disabled, or invalidated. The route
    /// stops rather than spinning on a dead handle.
    Lost,
}

/// The result of pulling from a source.
#[derive(Clone, Copy, Debug, Default)]
pub struct SourceRead {
    pub frames: usize,
    pub health: StreamHealth,
}

impl SourceRead {
    pub fn ok(frames: usize) -> Self {
        Self {
            frames,
            health: StreamHealth::Ok,
        }
    }

    pub fn lost() -> Self {
        Self {
            frames: 0,
            health: StreamHealth::Lost,
        }
    }
}

/// The result of pushing to a sink.
#[derive(Clone, Copy, Debug, Default)]
pub struct SinkWrite {
    pub frames: usize,
    pub health: StreamHealth,
}

impl SinkWrite {
    pub fn ok(frames: usize) -> Self {
        Self {
            frames,
            health: StreamHealth::Ok,
        }
    }

    pub fn lost() -> Self {
        Self {
            frames: 0,
            health: StreamHealth::Lost,
        }
    }
}

/// How full a device's own buffer is running.
///
/// Reported so the router can steer its resampler against clock drift. Two
/// endpoints nominally at 48 kHz drift apart over minutes; without this the
/// buffer between them slowly fills or empties until it breaks.
#[derive(Clone, Copy, Debug)]
pub struct Backlog {
    pub frames: usize,
    pub capacity: usize,
}

impl Backlog {
    /// Fill level as 0..=1.
    fn fill(self) -> f64 {
        if self.capacity == 0 {
            0.5
        } else {
            (self.frames as f64 / self.capacity as f64).clamp(0.0, 1.0)
        }
    }
}

/// Anything the router can pull interleaved `f32` from.
pub trait AudioSource: Send {
    /// The geometry of the audio this source produces. The router builds its
    /// buffers from this, so it must not change without a restart.
    fn format(&self) -> AudioFormat;

    /// Fill as much of `dst` as is available right now.
    ///
    /// Must not block: a source with nothing to give returns
    /// [`StreamHealth::Starved`] and zero frames rather than waiting.
    fn read(&mut self, dst: &mut [f32]) -> SourceRead;

    /// Drop anything buffered. Called after a discontinuity, where replaying
    /// pre-gap audio would place the stream's past at the wrong time.
    fn reset(&mut self) {}
}

/// Anything the router can push interleaved `f32` to.
pub trait AudioSink: Send {
    fn format(&self) -> AudioFormat;

    /// Accept as much of `src` as fits right now.
    ///
    /// Must not block. A short write is an overrun, counted by the router.
    fn write(&mut self, src: &[f32]) -> SinkWrite;

    /// How full this sink's own buffer is, if it can say. `None` disables
    /// drift correction for routes feeding it.
    fn backlog(&self) -> Option<Backlog> {
        None
    }

    fn reset(&mut self) {}
}

/// Something the router cannot do, explained rather than silently ignored.
///
/// §21.1 of the parity roadmap: never fake success. Every one of these is
/// returned before anything is mutated.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RouterError {
    #[error("no audio source is registered as {0:?}")]
    UnknownSource(SourceId),
    #[error("no audio destination is registered as {0:?}")]
    UnknownSink(SinkId),
    #[error("no effect is registered as {0:?}")]
    UnknownProcessor(ProcessorId),
    #[error("an audio source is already registered as {0:?}")]
    DuplicateSource(SourceId),
    #[error("an audio destination is already registered as {0:?}")]
    DuplicateSink(SinkId),
    #[error("an effect is already registered as {0:?}")]
    DuplicateProcessor(ProcessorId),
    #[error("two routes claim the identity {0:?}")]
    DuplicateRoute(RouteId),
    #[error("{0:?} already feeds another route")]
    SourceInUse(SourceId),
    #[error("{0:?} is used twice by the same route")]
    DuplicateDestination(SinkId),
    #[error("{0:?} carries no destination")]
    RouteWithoutDestination(RouteId),
    #[error("{0:?} is still used by a route")]
    StillRouted(RouteId),
    #[error("no route is installed as {0:?}")]
    UnknownRoute(RouteId),
    #[error("sample rate {sample_rate} with {channels} channels is not a usable audio format")]
    InvalidFormat { sample_rate: u32, channels: u16 },
    #[error(
        "{processor:?} was prepared for {prepared_rate} Hz / {prepared_channels} ch but the route \
         runs at {route_rate} Hz / {route_channels} ch"
    )]
    ProcessorFormatMismatch {
        processor: ProcessorId,
        prepared_rate: u32,
        prepared_channels: u16,
        route_rate: u32,
        route_channels: u16,
    },
    #[error(
        "{processor:?} was prepared for at most {prepared_frames} frames but the router uses \
         blocks of {block_frames}"
    )]
    ProcessorBlockTooSmall {
        processor: ProcessorId,
        prepared_frames: u32,
        block_frames: u32,
    },
    #[error("{0:?} is used twice by the same route")]
    DuplicateProcessorUse(ProcessorId),
    #[error("{0:?} is already used by another route")]
    ProcessorInUse(ProcessorId),
    #[error("effect setup failed: {0}")]
    Effect(#[from] EffectError),
}

/// One destination of a route, as the caller describes it.
#[derive(Clone, Copy, Debug)]
pub struct DestinationSpec {
    pub sink: SinkId,
    /// Linear gain applied to this destination only.
    pub gain: f32,
}

impl DestinationSpec {
    pub fn new(sink: SinkId) -> Self {
        Self { sink, gain: 1.0 }
    }
}

/// One processed path out of a route, as the caller describes it.
///
/// A branch is what makes "insert an effect into *this* link" mean what it
/// says. A source feeding both a plain destination and an effect-processed
/// one has two branches: the source is still pulled once per block, but each
/// branch gets its own copy of the audio and its own chain, so the effect
/// cannot leak into the sibling path.
#[derive(Clone, Debug)]
pub struct BranchSpec {
    /// Effects in order. Each may appear on exactly one branch, of one route.
    pub processors: Vec<ProcessorId>,
    /// Linear gain applied after this branch's effects.
    pub gain: f32,
    pub destinations: Vec<DestinationSpec>,
}

impl BranchSpec {
    pub fn to(destinations: Vec<DestinationSpec>) -> Self {
        Self {
            processors: Vec::new(),
            gain: 1.0,
            destinations,
        }
    }
}

/// A route, as the caller describes it.
///
/// The graph layer builds these from qpwgraph links; the router does not know
/// what a node is.
#[derive(Clone, Debug)]
pub struct RouteSpec {
    pub id: RouteId,
    pub source: SourceId,
    /// Linear gain applied to everything this source feeds, before any branch.
    ///
    /// Not clamped to 1.0. This is where Windows gets the >100% boost §13
    /// calls for: the endpoint's own volume control still tops out at unity,
    /// and software gain in a route qpwgraph owns is a separate, honest
    /// capability rather than a lie about the hardware.
    pub gain: f32,
    pub branches: Vec<BranchSpec>,
}

impl RouteSpec {
    /// The common case: one source, one destination, unity gain, no effects.
    pub fn direct(id: RouteId, source: SourceId, sink: SinkId) -> Self {
        Self {
            id,
            source,
            gain: 1.0,
            branches: vec![BranchSpec::to(vec![DestinationSpec::new(sink)])],
        }
    }

    /// Every destination this route reaches, across all its branches.
    pub fn destinations(&self) -> impl Iterator<Item = &DestinationSpec> {
        self.branches
            .iter()
            .flat_map(|branch| branch.destinations.iter())
    }
}

/// What one [`RouterCore::process`] call did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessReport {
    /// Frames of the router's clock this call advanced.
    pub frames: usize,
    /// Routes whose source or destination reported that its device is gone.
    /// The control layer reconciles these; the router does not reopen devices.
    pub lost: Vec<RouteId>,
}

/// How the router is configured. Fixed for the router's lifetime, because
/// every buffer is sized from it.
#[derive(Clone, Copy, Debug)]
pub struct RouterConfig {
    /// Frames each route pulls per cycle, at the source's own rate.
    pub block_frames: usize,
    /// Rate the router's frame clock runs at, used for meter ages. Not a
    /// resampling target; routes keep their own rates.
    pub clock_rate: u32,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            // 480 frames is 10 ms at 48 kHz, comfortably inside a WASAPI
            // shared-mode period and coarse enough that per-block overhead
            // stays negligible.
            block_frames: 480,
            clock_rate: 48_000,
        }
    }
}

/// Proportional gain of the drift controller, in ratio units per unit of
/// fill-level error. Small on purpose: correcting a full buffer in one block
/// would be audible, and the error it fights accumulates over minutes.
const DRIFT_GAIN: f64 = 20.0 / 1_000_000.0;

struct SourceSlot {
    source: Box<dyn AudioSource>,
    format: AudioFormat,
}

struct SinkSlot {
    sink: Box<dyn AudioSink>,
    format: AudioFormat,
}

struct ProcessorSlot {
    processor: Box<dyn EffectProcessor>,
    spec: AudioSpec,
    /// Bypassed effects keep their configuration and their place in the
    /// chain; §12.1 requires bypass not to destroy either.
    bypassed: bool,
}

/// One destination of a live route, with the buffers its conversion needs.
struct Destination {
    sink: SinkId,
    /// Index into the mix accumulators, resolved once at build time so the
    /// cycle never searches a map.
    mix: usize,
    gain: f32,
    map: ChannelMap,
    /// Source-rate audio already in the sink's channel geometry, waiting for
    /// the resampler. Carries the frames the resampler could not yet use.
    staging: Vec<f32>,
    staged_frames: usize,
    /// Resampler output for this block.
    converted: Vec<f32>,
    max_out_frames: usize,
    resampler: Resampler,
    sink_channels: u16,
}

/// One live processed path out of a route.
struct Branch {
    processors: Vec<ProcessorId>,
    gain: f32,
    destinations: Vec<Destination>,
    /// This branch's own copy of the block, so its effects cannot reach a
    /// sibling branch's audio.
    block: Vec<f32>,
    /// The input to the processor currently being called. It is preallocated
    /// so a processor that mutates its buffer before returning an error or
    /// panicking can still be bypassed transparently for this block.
    fallback: Vec<f32>,
    meter: Arc<MeterCell>,
}

struct Route {
    id: RouteId,
    source: SourceId,
    gain: f32,
    branches: Vec<Branch>,
    format: AudioFormat,
    /// Source-rate, source-geometry audio for this block, shared by every
    /// branch. The source is pulled exactly once however many branches read
    /// it.
    input: Vec<f32>,
    /// The level the source itself produced, before this route's gain and
    /// before any branch's effects.
    ///
    /// A separate reading from the branch meters on purpose. PipeWire meters
    /// a port with the audio that port carries, so the level shown on a
    /// microphone is what the microphone produced — not what something
    /// downstream did to it. This is that reading; the branch meters are what
    /// each destination receives.
    source_meter: Arc<MeterCell>,
    diagnostics: Arc<RouteDiagnostics>,
}

/// One sink's accumulator for a block.
struct Mix {
    sink: SinkId,
    buffer: Vec<f32>,
    frames: usize,
    channels: u16,
}

/// The installed route table plus its accumulators. Replaced as a unit.
#[derive(Default)]
struct RouteTable {
    routes: Vec<Route>,
    mixes: Vec<Mix>,
}

/// The engine.
///
/// Owns every source, sink, and effect, and the current route table. Single
/// threaded: see the module docs for why that is the whole design rather than
/// a limitation.
#[derive(Default)]
pub struct RouterCore {
    config: RouterConfig,
    sources: BTreeMap<SourceId, SourceSlot>,
    sinks: BTreeMap<SinkId, SinkSlot>,
    processors: BTreeMap<ProcessorId, ProcessorSlot>,
    table: RouteTable,
    /// One meter per branch: a route with an effect on one path and none on
    /// another has two different levels to report.
    meters: BTreeMap<(RouteId, usize), Arc<MeterCell>>,
    /// Per-route source levels: the audio each source produced, before the
    /// route touched it.
    source_meters: BTreeMap<RouteId, Arc<MeterCell>>,
    diagnostics: BTreeMap<RouteId, Arc<RouteDiagnostics>>,
    frame_clock: u64,
}

impl std::fmt::Debug for RouterCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterCore")
            .field("block_frames", &self.config.block_frames)
            .field("sources", &self.sources.len())
            .field("sinks", &self.sinks.len())
            .field("processors", &self.processors.len())
            .field("routes", &self.table.routes.len())
            .field("frame_clock", &self.frame_clock)
            .finish()
    }
}

impl RouterCore {
    pub fn new(config: RouterConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub fn config(&self) -> RouterConfig {
        self.config
    }

    /// Frames of audio the router has processed. Meter ages are measured
    /// against this rather than wall time, which is what makes them
    /// reproducible in tests.
    pub fn frame_clock(&self) -> u64 {
        self.frame_clock
    }

    /// Register a source. The route table is unchanged until
    /// [`RouterCore::set_routes`] refers to it.
    pub fn add_source(
        &mut self,
        id: SourceId,
        source: Box<dyn AudioSource>,
    ) -> Result<(), RouterError> {
        if self.sources.contains_key(&id) {
            return Err(RouterError::DuplicateSource(id));
        }
        let format = source.format();
        validate_format(format)?;
        self.sources.insert(id, SourceSlot { source, format });
        Ok(())
    }

    /// Register a destination.
    pub fn add_sink(&mut self, id: SinkId, sink: Box<dyn AudioSink>) -> Result<(), RouterError> {
        if self.sinks.contains_key(&id) {
            return Err(RouterError::DuplicateSink(id));
        }
        let format = sink.format();
        validate_format(format)?;
        self.sinks.insert(id, SinkSlot { sink, format });
        Ok(())
    }

    /// Register an effect instance, already prepared by the caller.
    ///
    /// Preparation is the caller's because it is the allocating step and the
    /// caller knows the route the effect is destined for. [`set_routes`] then
    /// refuses any route whose geometry disagrees with what the effect was
    /// prepared for, rather than handing a processor a buffer shape it never
    /// agreed to.
    ///
    /// [`set_routes`]: RouterCore::set_routes
    pub fn add_processor(
        &mut self,
        id: ProcessorId,
        mut processor: Box<dyn EffectProcessor>,
        spec: AudioSpec,
    ) -> Result<(), RouterError> {
        if self.processors.contains_key(&id) {
            return Err(RouterError::DuplicateProcessor(id));
        }
        spec.validate()?;
        processor.prepare(spec.clone())?;
        self.processors.insert(
            id,
            ProcessorSlot {
                processor,
                spec,
                bypassed: false,
            },
        );
        Ok(())
    }

    /// Control-thread diagnostics handle; audio callbacks only update atomics.
    pub fn hush_diagnostics(
        &self,
        id: ProcessorId,
    ) -> Option<Arc<pw_graph_effects::HushDiagnostics>> {
        self.processors
            .get(&id)
            .and_then(|slot| slot.processor.hush_diagnostics())
    }

    /// Remove a source, handing it back so it is dropped on the caller's
    /// thread rather than the router's.
    pub fn remove_source(&mut self, id: SourceId) -> Result<Box<dyn AudioSource>, RouterError> {
        if let Some(route) = self.table.routes.iter().find(|route| route.source == id) {
            return Err(RouterError::StillRouted(route.id));
        }
        self.sources
            .remove(&id)
            .map(|slot| slot.source)
            .ok_or(RouterError::UnknownSource(id))
    }

    /// Remove a destination, handing it back for disposal.
    pub fn remove_sink(&mut self, id: SinkId) -> Result<Box<dyn AudioSink>, RouterError> {
        if let Some(route) = self.table.routes.iter().find(|route| {
            route.branches.iter().any(|branch| {
                branch
                    .destinations
                    .iter()
                    .any(|destination| destination.sink == id)
            })
        }) {
            return Err(RouterError::StillRouted(route.id));
        }
        self.sinks
            .remove(&id)
            .map(|slot| slot.sink)
            .ok_or(RouterError::UnknownSink(id))
    }

    /// Remove an effect, handing it back for disposal.
    pub fn remove_processor(
        &mut self,
        id: ProcessorId,
    ) -> Result<Box<dyn EffectProcessor>, RouterError> {
        if let Some(route) = self.table.routes.iter().find(|route| {
            route
                .branches
                .iter()
                .any(|branch| branch.processors.contains(&id))
        }) {
            return Err(RouterError::StillRouted(route.id));
        }
        self.processors
            .remove(&id)
            .map(|slot| slot.processor)
            .ok_or(RouterError::UnknownProcessor(id))
    }

    /// Change one of an effect's parameters. Takes effect on the next block.
    pub fn set_processor_parameter(
        &mut self,
        id: ProcessorId,
        parameter: &str,
        value: f32,
    ) -> Result<(), RouterError> {
        let slot = self
            .processors
            .get_mut(&id)
            .ok_or(RouterError::UnknownProcessor(id))?;
        slot.processor.set_parameter(parameter, value)?;
        Ok(())
    }

    /// Bypass or re-enable an effect without disturbing its configuration or
    /// its position in the chain.
    pub fn set_processor_bypassed(
        &mut self,
        id: ProcessorId,
        bypassed: bool,
    ) -> Result<(), RouterError> {
        let slot = self
            .processors
            .get_mut(&id)
            .ok_or(RouterError::UnknownProcessor(id))?;
        if slot.bypassed != bypassed {
            slot.bypassed = bypassed;
            // Coming out of bypass with a tail from before the gap would play
            // audio that never went in.
            if !slot.processor.set_host_bypass(bypassed) {
                slot.processor.reset();
            }
        }
        Ok(())
    }

    pub fn is_processor_bypassed(&self, id: ProcessorId) -> Option<bool> {
        self.processors.get(&id).map(|slot| slot.bypassed)
    }

    /// Replace the whole route table, atomically.
    ///
    /// Every route is validated and every buffer allocated before the live
    /// table is touched, so a rejected table leaves the previous one running
    /// untouched. That is the rollback guarantee the shared command layer
    /// depends on for reroute.
    pub fn set_routes(&mut self, specs: &[RouteSpec]) -> Result<(), RouterError> {
        let mut seen_routes = BTreeSet::new();
        let mut claimed_sources = BTreeSet::new();
        let mut claimed_processors = BTreeSet::new();
        // Mix accumulators are keyed by sink and shared by every route that
        // feeds it, which is what makes several sources into one destination
        // a mix instead of a race.
        let mut mix_index: BTreeMap<SinkId, usize> = BTreeMap::new();
        let mut mixes: Vec<Mix> = Vec::new();
        let mut routes = Vec::with_capacity(specs.len());

        for spec in specs {
            if !seen_routes.insert(spec.id) {
                return Err(RouterError::DuplicateRoute(spec.id));
            }
            if !claimed_sources.insert(spec.source) {
                return Err(RouterError::SourceInUse(spec.source));
            }
            let source_format = self
                .sources
                .get(&spec.source)
                .ok_or(RouterError::UnknownSource(spec.source))?
                .format;
            if spec
                .branches
                .iter()
                .all(|branch| branch.destinations.is_empty())
            {
                return Err(RouterError::RouteWithoutDestination(spec.id));
            }
            // A sink may be fed by several routes -- that is a mix -- but not
            // twice by one source, which would simply double it.
            let mut seen_sinks = BTreeSet::new();

            let mut branches = Vec::with_capacity(spec.branches.len());
            for (index, branch) in spec.branches.iter().enumerate() {
                if branch.destinations.is_empty() {
                    return Err(RouterError::RouteWithoutDestination(spec.id));
                }

                let mut branch_processors = Vec::with_capacity(branch.processors.len());
                let mut seen_processors = BTreeSet::new();
                for &processor in &branch.processors {
                    let slot = self
                        .processors
                        .get(&processor)
                        .ok_or(RouterError::UnknownProcessor(processor))?;
                    if !seen_processors.insert(processor) {
                        return Err(RouterError::DuplicateProcessorUse(processor));
                    }
                    if !claimed_processors.insert(processor) {
                        return Err(RouterError::ProcessorInUse(processor));
                    }
                    if slot.spec.sample_rate != source_format.sample_rate
                        || slot.spec.channels != source_format.channels
                    {
                        return Err(RouterError::ProcessorFormatMismatch {
                            processor,
                            prepared_rate: slot.spec.sample_rate,
                            prepared_channels: slot.spec.channels,
                            route_rate: source_format.sample_rate,
                            route_channels: source_format.channels,
                        });
                    }
                    if (slot.spec.max_frames as usize) < self.config.block_frames {
                        return Err(RouterError::ProcessorBlockTooSmall {
                            processor,
                            prepared_frames: slot.spec.max_frames,
                            block_frames: self.config.block_frames as u32,
                        });
                    }
                    branch_processors.push(processor);
                }

                let mut destinations = Vec::with_capacity(branch.destinations.len());
                for destination in &branch.destinations {
                    let sink_format = self
                        .sinks
                        .get(&destination.sink)
                        .ok_or(RouterError::UnknownSink(destination.sink))?
                        .format;
                    if !seen_sinks.insert(destination.sink) {
                        return Err(RouterError::DuplicateDestination(destination.sink));
                    }
                    let max_out_frames =
                        source_format.resampled_capacity(sink_format, self.config.block_frames);
                    let mix = *mix_index.entry(destination.sink).or_insert_with(|| {
                        mixes.push(Mix {
                            sink: destination.sink,
                            buffer: Vec::new(),
                            frames: 0,
                            channels: sink_format.channels,
                        });
                        mixes.len() - 1
                    });
                    // The accumulator has to hold the loudest demand of any
                    // route feeding it, not just this one.
                    let wanted = sink_format.samples(max_out_frames);
                    if mixes[mix].buffer.len() < wanted {
                        mixes[mix].buffer.resize(wanted, 0.0);
                    }
                    destinations.push(Destination {
                        sink: destination.sink,
                        mix,
                        gain: destination.gain,
                        map: ChannelMap::between(source_format.channels, sink_format.channels),
                        // Two blocks of headroom: the resampler leaves at most
                        // a fraction of a frame behind per block, and doubling
                        // means the staging never has to grow while audio is
                        // running.
                        staging: vec![0.0; sink_format.samples(self.config.block_frames * 2)],
                        staged_frames: 0,
                        converted: vec![0.0; sink_format.samples(max_out_frames)],
                        max_out_frames,
                        resampler: Resampler::new(
                            AudioFormat::new(source_format.sample_rate, sink_format.channels),
                            sink_format,
                        ),
                        sink_channels: sink_format.channels,
                    });
                }

                // Meters survive a route table replacement, so a reroute does
                // not blank the UI's level bars.
                let meter = Arc::clone(
                    self.meters
                        .entry((spec.id, index))
                        .or_insert_with(|| Arc::new(MeterCell::new())),
                );
                branches.push(Branch {
                    processors: branch_processors,
                    gain: branch.gain,
                    destinations,
                    block: vec![0.0; source_format.samples(self.config.block_frames)],
                    fallback: vec![0.0; source_format.samples(self.config.block_frames)],
                    meter,
                });
            }

            // Counters survive too: the dropout history was about to explain
            // why the user rerouted.
            let diagnostics = Arc::clone(self.diagnostics.entry(spec.id).or_default());
            let source_meter = Arc::clone(
                self.source_meters
                    .entry(spec.id)
                    .or_insert_with(|| Arc::new(MeterCell::new())),
            );

            routes.push(Route {
                id: spec.id,
                source: spec.source,
                gain: spec.gain,
                branches,
                format: source_format,
                input: vec![0.0; source_format.samples(self.config.block_frames)],
                source_meter,
                diagnostics,
            });
        }

        self.table = RouteTable { routes, mixes };
        // Meters for branches that no longer exist would otherwise keep
        // reporting the last level they saw.
        let live_branches = self.live_branches();
        for (key, meter) in &self.meters {
            if !live_branches.contains(key) {
                meter.clear();
            }
        }
        let live: BTreeSet<RouteId> = self.table.routes.iter().map(|route| route.id).collect();
        for (id, meter) in &self.source_meters {
            if !live.contains(id) {
                meter.clear();
            }
        }
        Ok(())
    }

    fn live_branches(&self) -> BTreeSet<(RouteId, usize)> {
        self.table
            .routes
            .iter()
            .flat_map(|route| (0..route.branches.len()).map(move |branch| (route.id, branch)))
            .collect()
    }

    /// Ids of the routes currently installed, in table order.
    pub fn route_ids(&self) -> Vec<RouteId> {
        self.table.routes.iter().map(|route| route.id).collect()
    }

    /// The current level at a route's output, or `None` if that route has
    /// never existed.
    ///
    /// A route with several branches reports its first; the others are
    /// reachable through [`RouterCore::branch_meter`]. Most routes have
    /// exactly one, so this is the level after that branch's effects — the
    /// post-effect reading the parity contract asks for.
    pub fn meter(&self, route: RouteId) -> Option<MeterReading> {
        self.branch_meter(route, 0)
    }

    /// The level the route's source produced, before the route touched it.
    ///
    /// This is the reading that belongs on the device's own node: what the
    /// microphone captured, not what an effect downstream made of it. Peak
    /// *and* RMS, which is the whole point — Core Audio's endpoint meter has
    /// no RMS to give.
    pub fn source_meter(&self, route: RouteId) -> Option<MeterReading> {
        self.source_meters
            .get(&route)
            .map(|meter| meter.read(self.frame_clock, self.config.clock_rate))
    }

    /// The level leaving one branch of a route, after its effects.
    pub fn branch_meter(&self, route: RouteId, branch: usize) -> Option<MeterReading> {
        self.meters
            .get(&(route, branch))
            .map(|meter| meter.read(self.frame_clock, self.config.clock_rate))
    }

    /// A route's counters.
    pub fn metrics(&self, route: RouteId) -> Option<RouteMetrics> {
        self.diagnostics
            .get(&route)
            .map(|diagnostics| diagnostics.snapshot())
    }

    /// Forget meters and counters for routes that are no longer installed.
    ///
    /// Kept separate from [`RouterCore::set_routes`] so that a reroute --
    /// remove then re-add within one transaction -- does not lose the history
    /// that explains what went wrong.
    pub fn forget_retired_routes(&mut self) {
        let live_branches = self.live_branches();
        let live: BTreeSet<RouteId> = self.table.routes.iter().map(|route| route.id).collect();
        self.meters.retain(|key, _| live_branches.contains(key));
        self.source_meters.retain(|id, _| live.contains(id));
        self.diagnostics.retain(|id, _| live.contains(id));
    }

    /// Move one block of audio.
    ///
    /// Allocates nothing, locks nothing, and returns whatever went wrong as
    /// counters rather than as errors: a dropout is not a reason to stop the
    /// other routes.
    pub fn process(&mut self) -> ProcessReport {
        let block_frames = self.config.block_frames;
        let mut report = ProcessReport {
            frames: block_frames,
            lost: Vec::new(),
        };

        // Disjoint field borrows: the cycle walks routes while indexing into
        // the source, sink, and effect registries, which the borrow checker
        // only allows because they are separate fields.
        let RouterCore {
            sources,
            sinks,
            processors,
            table,
            frame_clock,
            ..
        } = self;
        let RouteTable { routes, mixes } = table;

        // Zeroing the whole accumulator up front is what makes a mix a sum
        // rather than a sum layered over last block's tail. It is a memset of
        // a few kilobytes per sink, which is far cheaper than tracking which
        // sub-ranges each contributor touched.
        for mix in mixes.iter_mut() {
            mix.buffer.fill(0.0);
            mix.frames = 0;
        }

        for route in routes.iter_mut() {
            let route_started = Instant::now();
            let Some(slot) = sources.get_mut(&route.source) else {
                // A source cannot vanish while routed -- `remove_source`
                // refuses -- so this is unreachable rather than tolerated.
                route.diagnostics.set_fault(RouteFault::DeviceLost);
                report.lost.push(route.id);
                continue;
            };

            let wanted = route.format.samples(block_frames);
            let read = slot.source.read(&mut route.input[..wanted]);
            if read.health == StreamHealth::Lost {
                route.diagnostics.set_fault(RouteFault::DeviceLost);
                report.lost.push(route.id);
                continue;
            }
            let frames = read.frames.min(block_frames);
            if frames < block_frames {
                route
                    .diagnostics
                    .note_source_underrun((block_frames - frames) as u64);
                // A partial block is already a dropout; waiting for a fully
                // empty one to say so would hide the interesting case, where
                // a device is keeping up almost but not quite.
                route.diagnostics.set_fault(RouteFault::SourceStarved);
                if frames == 0 {
                    // Nothing to carry: leave the destinations alone so a
                    // starved route contributes silence to a mix rather than
                    // the previous block again.
                    continue;
                }
            }
            let samples = route.format.samples(frames);
            for sample in &mut route.input[..samples] {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
            }
            let input = &route.input[..samples];
            // The device's own level, before this route touched it. This is
            // the reading an endpoint node shows, and the one Core Audio can
            // only give as a peak.
            route.source_meter.observe(input, *frame_clock);

            let mut queue_depth = 0u64;
            let mut ratio = 1.0;
            let mut drift_ppm = 0.0;
            let mut destination_backed_up = false;
            let mut processor_failed = false;
            let mut effect_us = 0u64;

            for branch in route.branches.iter_mut() {
                // Each branch works on its own copy, which is what keeps an
                // effect inserted into one link out of a sibling fan-out.
                let block = &mut branch.block[..samples];
                block.copy_from_slice(input);
                if route.gain != 1.0 {
                    for sample in block.iter_mut() {
                        *sample *= route.gain;
                    }
                }

                let effects_started = Instant::now();
                for id in &branch.processors {
                    let Some(slot) = processors.get_mut(id) else {
                        continue;
                    };
                    let aligned_bypass = slot.processor.set_host_bypass(slot.bypassed);
                    if slot.bypassed && !aligned_bypass {
                        continue;
                    }
                    branch.fallback[..samples].copy_from_slice(block);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        slot.processor.process(block, frames as u32)
                    }));
                    if !matches!(result, Ok(Ok(())))
                        || !block.iter().all(|sample| sample.is_finite())
                    {
                        // A failing effect is bypassed for this block. Restore
                        // the input it received even if it partially mutated
                        // the buffer or panicked; never publish a stale,
                        // non-finite, or half-processed block.
                        block.copy_from_slice(&branch.fallback[..samples]);
                        processor_failed = true;
                    } else if slot.processor.has_failed() {
                        // Stateful workers can retain an aligned dry fallback
                        // after reporting a persistent failure. Keep that
                        // audio and surface the route fault separately.
                        processor_failed = true;
                    }
                }
                effect_us += effects_started.elapsed().as_micros() as u64;

                if branch.gain != 1.0 {
                    for sample in block.iter_mut() {
                        *sample *= branch.gain;
                    }
                }

                // Post-effect, at the branch's output: what its destinations
                // are about to receive.
                branch.meter.observe(block, *frame_clock);

                let block = &branch.block[..samples];
                for destination in branch.destinations.iter_mut() {
                    let channels = destination.sink_channels as usize;
                    let staged = destination.staged_frames;
                    let staging_capacity = destination.staging.len() / channels;
                    let room = staging_capacity.saturating_sub(staged);
                    let mapped = destination.map.apply(
                        block,
                        route.format.channels,
                        &mut destination.staging[staged * channels..(staged + room) * channels],
                        destination.sink_channels,
                    );
                    if mapped < frames {
                        // Only reachable if the staging never drained, which means
                        // the destination has been refusing audio for blocks.
                        route
                            .diagnostics
                            .note_source_overrun((frames - mapped) as u64);
                    }
                    destination.staged_frames = staged + mapped;

                    let available = destination.staged_frames * channels;
                    let out_limit = destination.max_out_frames * channels;
                    let converted = if destination.resampler.is_passthrough() {
                        // Matching rates with no drift correction in play: copy,
                        // so a same-rate route is sample-exact and adds no
                        // latency. Interpolating at a ratio of exactly 1.0 would
                        // reproduce the input anyway, but only after holding a
                        // frame back for the next block's right-hand side.
                        let frames = destination.staged_frames.min(destination.max_out_frames);
                        let samples = frames * channels;
                        destination.converted[..samples]
                            .copy_from_slice(&destination.staging[..samples]);
                        super::resample::Converted {
                            consumed: frames,
                            produced: frames,
                        }
                    } else {
                        destination.resampler.process(
                            &destination.staging[..available],
                            &mut destination.converted[..out_limit],
                        )
                    };
                    if converted.consumed > 0 {
                        destination
                            .staging
                            .copy_within(converted.consumed * channels..available, 0);
                        destination.staged_frames -= converted.consumed;
                    }
                    queue_depth += destination.staged_frames as u64;
                    ratio = destination.resampler.ratio();
                    drift_ppm = destination.resampler.drift_ppm();

                    if converted.produced == 0 {
                        continue;
                    }
                    let produced_samples = converted.produced * channels;
                    if destination.gain != 1.0 {
                        for sample in destination.converted[..produced_samples].iter_mut() {
                            *sample *= destination.gain;
                        }
                    }
                    let mix = &mut mixes[destination.mix];
                    let room = mix.buffer.len().min(produced_samples);
                    for (accumulated, &sample) in mix.buffer[..room]
                        .iter_mut()
                        .zip(destination.converted[..room].iter())
                    {
                        *accumulated += sample;
                    }
                    // The accumulator is sized for the loudest demand of any
                    // contributor, so this is a sizing bug rather than a device
                    // problem -- but silently truncating audio is not acceptable
                    // either way, so it is counted.
                    if produced_samples > room {
                        destination_backed_up = true;
                        route
                            .diagnostics
                            .note_sink_overrun(((produced_samples - room) / channels) as u64);
                    }
                    mix.frames = mix.frames.max(room / channels);
                }
            }

            route.diagnostics.set_queue_depth(queue_depth);
            route.diagnostics.set_conversion(ratio, drift_ppm);
            route.diagnostics.note_frames(frames as u64);
            route
                .diagnostics
                .set_timings(route_started.elapsed().as_micros() as u64, effect_us);
            if processor_failed {
                route.diagnostics.set_fault(RouteFault::ProcessorFailed);
            } else if destination_backed_up {
                route.diagnostics.set_fault(RouteFault::SinkBackedUp);
            } else if frames == block_frames {
                route.diagnostics.clear_fault();
            }
        }

        for mix in mixes.iter_mut() {
            let Some(slot) = sinks.get_mut(&mix.sink) else {
                continue;
            };
            let samples = mix.frames * mix.channels as usize;
            if samples == 0 {
                continue;
            }
            let written = slot.sink.write(&mix.buffer[..samples]);
            let backlog = slot.sink.backlog();
            for route in routes.iter_mut() {
                let Some(destination) = route
                    .branches
                    .iter_mut()
                    .flat_map(|branch| branch.destinations.iter_mut())
                    .find(|destination| destination.sink == mix.sink)
                else {
                    continue;
                };
                if written.health == StreamHealth::Lost {
                    route.diagnostics.set_fault(RouteFault::DeviceLost);
                    if !report.lost.contains(&route.id) {
                        report.lost.push(route.id);
                    }
                    continue;
                }
                if written.frames < mix.frames {
                    route
                        .diagnostics
                        .note_sink_overrun((mix.frames - written.frames) as u64);
                    route.diagnostics.set_fault(RouteFault::SinkBackedUp);
                }
                // Steer the conversion so the destination's own buffer settles
                // at half full: too empty and it will underrun on the next
                // hiccup, too full and latency grows for no benefit.
                if let Some(backlog) = backlog {
                    destination
                        .resampler
                        .set_drift((backlog.fill() - 0.5) * DRIFT_GAIN * 2.0);
                }
            }
        }

        *frame_clock += block_frames as u64;
        report
    }

    /// Reset every device and conversion on a route after a discontinuity.
    ///
    /// Counted, so a route that keeps needing this is visible rather than
    /// merely quiet.
    pub fn reset_route(&mut self, id: RouteId) -> Result<(), RouterError> {
        let Some(route) = self.table.routes.iter_mut().find(|route| route.id == id) else {
            return Err(RouterError::UnknownRoute(id));
        };
        if let Some(slot) = self.sources.get_mut(&route.source) {
            slot.source.reset();
        }
        for branch in route.branches.iter_mut() {
            for destination in branch.destinations.iter_mut() {
                destination.resampler.reset();
                destination.staged_frames = 0;
                if let Some(slot) = self.sinks.get_mut(&destination.sink) {
                    slot.sink.reset();
                }
            }
            for id in &branch.processors {
                if let Some(slot) = self.processors.get_mut(id) {
                    slot.processor.reset();
                }
            }
        }
        route.diagnostics.note_discontinuity();
        route.diagnostics.note_restart();
        Ok(())
    }
}

fn validate_format(format: AudioFormat) -> Result<(), RouterError> {
    if format.is_valid() {
        Ok(())
    } else {
        Err(RouterError::InvalidFormat {
            sample_rate: format.sample_rate,
            channels: format.channels,
        })
    }
}
