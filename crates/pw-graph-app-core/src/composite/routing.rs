//! Which child backend owns a graph object, and the mutation that follows.
//!
//! Ownership is read from the shared ID namespace, never guessed: a mixed
//! pair is rejected rather than falling through to a default backend.
//!
//! The `route_*` methods are what the `GraphDriver` impl delegates to. Each
//! keeps its `#[cfg]` arms written out, so a reader can see which native
//! backend actually runs for a given route. That repetition is deliberate --
//! a closure or macro over the arms would hide exactly that.

use super::*;

/// The native driver that owns a pair of graph ports.  This classification is
/// kept independent of the UI and is used before a mutation is forwarded to a
/// child driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositeRoute {
    PipeWire,
    AlsaMidi,
    WindowsAudio,
    WindowsMidi,
    Demo,
}

impl CompositeRoute {
    pub(super) fn from_backend(backend: BackendKind) -> Self {
        match backend {
            BackendKind::PipeWire => Self::PipeWire,
            BackendKind::AlsaMidi => Self::AlsaMidi,
            BackendKind::WindowsAudio => Self::WindowsAudio,
            BackendKind::WindowsMidi => Self::WindowsMidi,
            BackendKind::Demo => Self::Demo,
        }
    }
}

/// Classify a connection without touching either native backend.
///
/// Backend ownership is read from the shared ID namespace. A mixed pair is
/// intentionally rejected instead of being allowed to fall through to a
/// default native backend.
pub fn route_for_ports(src: PortId, dst: PortId) -> Result<CompositeRoute, &'static str> {
    let source = backend_for_port(src).ok_or("source port has an unknown backend namespace")?;
    let destination =
        backend_for_port(dst).ok_or("destination port has an unknown backend namespace")?;
    if source != destination {
        return Err(match (source, destination) {
            (BackendKind::PipeWire, BackendKind::AlsaMidi)
            | (BackendKind::AlsaMidi, BackendKind::PipeWire) => {
                "connections cannot cross PipeWire and ALSA MIDI backends"
            }
            _ => "connections cannot cross native backends",
        });
    }
    Ok(CompositeRoute::from_backend(source))
}

impl CompositeDriver {
    pub(super) fn unsupported(message: impl Into<String>) -> BackendError {
        BackendError::Unsupported(message.into())
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    pub(super) fn pipewire_mut(&mut self) -> BackendResult<&mut PipewireDriver> {
        self.pipewire
            .as_mut()
            .ok_or_else(|| Self::unsupported("PipeWire backend is unavailable"))
    }

    #[cfg(all(target_os = "linux", feature = "alsa"))]
    pub(super) fn alsa_mut(&mut self) -> BackendResult<&mut AlsaMidiDriver> {
        self.alsa
            .as_mut()
            .ok_or_else(|| Self::unsupported("ALSA MIDI backend is unavailable"))
    }

    #[allow(dead_code)]
    pub(super) fn rebuild_after_native_mutation(&mut self) {
        if let Err(error) = self.rebuild_merged_graph() {
            // The native mutation has already succeeded.  Keep that success
            // visible and let the next normal refresh repair the projection.
            eprintln!("could not rebuild composite graph: {error}");
        }
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    pub(super) fn mutate_pipewire<T>(
        &mut self,
        operation: impl FnOnce(&mut PipewireDriver) -> BackendResult<T>,
    ) -> BackendResult<T> {
        let value = operation(self.pipewire_mut()?)?;
        self.rebuild_after_native_mutation();
        Ok(value)
    }

    /// Capabilities for one backend namespace in this composite.
    pub fn capabilities_for_backend(&self, backend: BackendKind) -> BackendCapabilities {
        match backend {
            BackendKind::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    self.pipewire
                        .as_ref()
                        .map(GraphDriver::capabilities)
                        .unwrap_or_default()
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                {
                    BackendCapabilities::default()
                }
            }
            BackendKind::AlsaMidi => {
                #[cfg(all(target_os = "linux", feature = "alsa"))]
                {
                    self.alsa
                        .as_ref()
                        .map(GraphDriver::capabilities)
                        .unwrap_or_default()
                }
                #[cfg(not(all(target_os = "linux", feature = "alsa")))]
                {
                    BackendCapabilities::default()
                }
            }
            BackendKind::WindowsAudio | BackendKind::WindowsMidi | BackendKind::Demo => {
                #[cfg(target_os = "windows")]
                if backend == BackendKind::WindowsAudio {
                    return self
                        .windows_audio
                        .as_ref()
                        .map(GraphDriver::capabilities)
                        .unwrap_or_default();
                }
                #[cfg(target_os = "windows")]
                if backend == BackendKind::WindowsMidi {
                    return self
                        .windows_midi
                        .as_ref()
                        .map(GraphDriver::capabilities)
                        .unwrap_or_default();
                }
                BackendCapabilities::default()
            }
        }
    }

    /// Capabilities for the backend that owns a graph node.
    ///
    /// Routing is then narrowed to what *this* node can do. A backend-wide
    /// `connect` is a union across the resources it owns: the Windows audio
    /// driver can rewire endpoints but not application sessions, so asking
    /// only the backend would light up a gesture on a session pin that could
    /// never succeed.
    pub fn capabilities_for_node(&self, node: NodeId) -> BackendCapabilities {
        let Some(backend) = backend_for_node(node) else {
            return BackendCapabilities::default();
        };
        let mut capabilities = self.capabilities_for_backend(backend);
        if !self.child_node_supports_routing(backend, node) {
            capabilities.connect = false;
            capabilities.disconnect = false;
        }
        capabilities
    }

    pub(super) fn child_node_supports_routing(&self, backend: BackendKind, node: NodeId) -> bool {
        // Minimal builds compile out every native backend below, leaving
        // `node` (and `self`) unused. Bind them explicitly so
        // `RUSTFLAGS="-D warnings"` stays green on Linux minimal checks.
        let _ = (&self, node);
        match backend {
            BackendKind::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    return self
                        .pipewire
                        .as_ref()
                        .is_some_and(|driver| driver.node_supports_routing(node));
                }
            }
            BackendKind::AlsaMidi => {
                #[cfg(all(target_os = "linux", feature = "alsa"))]
                {
                    return self
                        .alsa
                        .as_ref()
                        .is_some_and(|driver| driver.node_supports_routing(node));
                }
            }
            BackendKind::WindowsAudio => {
                #[cfg(target_os = "windows")]
                {
                    return self
                        .windows_audio
                        .as_ref()
                        .is_some_and(|driver| driver.node_supports_routing(node));
                }
            }
            BackendKind::WindowsMidi => {
                #[cfg(target_os = "windows")]
                {
                    return self
                        .windows_midi
                        .as_ref()
                        .is_some_and(|driver| driver.node_supports_routing(node));
                }
            }
            // A node whose child backend is not compiled in, or the demo
            // resources, which are not part of the live composite.
            _ => {}
        }
        false
    }

    /// Return whether a projected link can be persisted and mutated by the
    /// native child backend.
    pub fn is_link_mutable(&self, link: LinkId) -> bool {
        self.route_is_link_mutable(link)
    }
    pub(super) fn route_connect(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        match route_for_ports(src, dst) {
            Ok(CompositeRoute::AlsaMidi) => {
                #[cfg(all(target_os = "linux", feature = "alsa"))]
                {
                    let link = self.alsa_mut()?.connect(src, dst)?;
                    self.refresh()?;
                    Ok(link)
                }
                #[cfg(not(all(target_os = "linux", feature = "alsa")))]
                Err(Self::unsupported("ALSA MIDI backend is disabled"))
            }
            Ok(CompositeRoute::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    let link = self.pipewire_mut()?.connect(src, dst)?;
                    self.refresh()?;
                    Ok(link)
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                Err(Self::unsupported("PipeWire backend is disabled"))
            }
            Ok(CompositeRoute::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    let link = self
                        .windows_audio
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                        .connect(src, dst)?;
                    self.refresh()?;
                    Ok(link)
                }
                #[cfg(not(target_os = "windows"))]
                Err(Self::unsupported("Windows audio backend is unavailable"))
            }
            Ok(CompositeRoute::WindowsMidi) => {
                #[cfg(target_os = "windows")]
                {
                    let link = self
                        .windows_midi
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows MIDI backend is unavailable"))?
                        .connect(src, dst)?;
                    self.refresh()?;
                    Ok(link)
                }
                #[cfg(not(target_os = "windows"))]
                Err(Self::unsupported("Windows MIDI backend is unavailable"))
            }
            Ok(CompositeRoute::Demo) => Err(Self::unsupported(
                "demo resources are not part of the live composite",
            )),
            Err(error) => Err(Self::unsupported(error)),
        }
    }

    pub(super) fn route_disconnect(&mut self, link: LinkId) -> BackendResult<Link> {
        let existing = self
            .graph
            .link(link)
            .cloned()
            .ok_or(GraphError::MissingLink(link))?;
        let route = backend_for_link(link)
            .map(CompositeRoute::from_backend)
            .or_else(|| route_for_ports(existing.output_port, existing.input_port).ok())
            .ok_or_else(|| Self::unsupported("link has an unknown backend namespace"))?;
        match route {
            CompositeRoute::AlsaMidi => {
                #[cfg(all(target_os = "linux", feature = "alsa"))]
                {
                    self.alsa_mut()?.disconnect(link)?;
                    self.refresh()?;
                    Ok(existing)
                }
                #[cfg(not(all(target_os = "linux", feature = "alsa")))]
                Err(Self::unsupported("ALSA MIDI backend is disabled"))
            }
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    self.pipewire_mut()?.disconnect(link)?;
                    self.refresh()?;
                    Ok(existing)
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                Err(Self::unsupported("PipeWire backend is disabled"))
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                {
                    let removed = self
                        .windows_audio
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                        .disconnect(link)?;
                    self.refresh()?;
                    Ok(removed)
                }
                #[cfg(not(target_os = "windows"))]
                Err(Self::unsupported("Windows audio backend is unavailable"))
            }
            CompositeRoute::WindowsMidi => {
                #[cfg(target_os = "windows")]
                {
                    let removed = self
                        .windows_midi
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows MIDI backend is unavailable"))?
                        .disconnect(link)?;
                    self.refresh()?;
                    Ok(removed)
                }
                #[cfg(not(target_os = "windows"))]
                Err(Self::unsupported("Windows MIDI backend is unavailable"))
            }
            CompositeRoute::Demo => Err(Self::unsupported(
                "demo resources are not part of the live composite",
            )),
        }
    }

    pub(super) fn route_is_link_mutable(&self, link: LinkId) -> bool {
        let Some(existing) = self.graph.link(link) else {
            return false;
        };
        let route = backend_for_link(link)
            .map(CompositeRoute::from_backend)
            .or_else(|| route_for_ports(existing.output_port, existing.input_port).ok());
        match route {
            Some(CompositeRoute::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    return self
                        .pipewire
                        .as_ref()
                        .is_some_and(|driver| driver.is_link_mutable(link));
                }
            }
            Some(CompositeRoute::AlsaMidi) => {
                #[cfg(all(target_os = "linux", feature = "alsa"))]
                {
                    return self
                        .alsa
                        .as_ref()
                        .is_some_and(|driver| driver.is_link_mutable(link));
                }
            }
            Some(CompositeRoute::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    return self
                        .windows_audio
                        .as_ref()
                        .is_some_and(|driver| driver.is_link_mutable(link));
                }
            }
            Some(CompositeRoute::WindowsMidi) => {
                #[cfg(target_os = "windows")]
                {
                    return self
                        .windows_midi
                        .as_ref()
                        .is_some_and(|driver| driver.is_link_mutable(link));
                }
            }
            Some(CompositeRoute::Demo) | None => {}
        }
        false
    }

    pub(super) fn route_set_node_position(
        &mut self,
        node: NodeId,
        position: [f32; 2],
    ) -> BackendResult<()> {
        let _ = position;
        match backend_for_node(node) {
            Some(BackendKind::AlsaMidi) => {
                #[cfg(all(target_os = "linux", feature = "alsa"))]
                {
                    self.alsa_mut()?.set_node_position(node, position)?;
                    if let Some(node_data) = self.graph.nodes.get_mut(&node) {
                        node_data.position = position;
                    }
                    Ok(())
                }
                #[cfg(not(all(target_os = "linux", feature = "alsa")))]
                Err(Self::unsupported("ALSA MIDI backend is disabled"))
            }
            Some(BackendKind::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    self.pipewire_mut()?.set_node_position(node, position)?;
                    if let Some(node_data) = self.graph.nodes.get_mut(&node) {
                        node_data.position = position;
                    }
                    Ok(())
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                Err(Self::unsupported("PipeWire backend is disabled"))
            }
            Some(BackendKind::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    self.windows_audio
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                        .set_node_position(node, position)?;
                    if let Some(node_data) = self.graph.nodes.get_mut(&node) {
                        node_data.position = position;
                    }
                    Ok(())
                }
                #[cfg(not(target_os = "windows"))]
                {
                    Err(Self::unsupported("Windows audio backend is unavailable"))
                }
            }
            Some(BackendKind::WindowsMidi) => {
                #[cfg(target_os = "windows")]
                {
                    self.windows_midi
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows MIDI backend is unavailable"))?
                        .set_node_position(node, position)?;
                    if let Some(node_data) = self.graph.nodes.get_mut(&node) {
                        node_data.position = position;
                    }
                    Ok(())
                }
                #[cfg(not(target_os = "windows"))]
                {
                    Err(Self::unsupported("Windows MIDI backend is unavailable"))
                }
            }
            Some(BackendKind::Demo) => Err(Self::unsupported(
                "demo resources are not part of the live composite",
            )),
            None => Err(Self::unsupported("node has an unknown backend namespace")),
        }
    }

    /// Forwarded to the backend that owns the node.
    ///
    /// This must stay in sync with `set_node_volume`/`set_node_mute`: the trait
    /// default answers `UNSUPPORTED`, so a composite that forgets to delegate
    /// silently strips every audio control off every live card instead of
    /// failing loudly. Covered by `composite_forwards_audio_state_to_the_owning_backend`.
    pub(super) fn route_node_audio_state(&self, node: NodeId) -> BackendResult<NodeAudioState> {
        match backend_for_node(node) {
            // Real nodes with nothing to control, not errors.
            Some(BackendKind::AlsaMidi) | Some(BackendKind::WindowsMidi) => {
                Ok(NodeAudioState::UNSUPPORTED)
            }
            Some(BackendKind::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    match self.pipewire.as_ref() {
                        Some(driver) => driver.node_audio_state(node),
                        None => Ok(NodeAudioState::UNSUPPORTED),
                    }
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                {
                    let _ = node;
                    Ok(NodeAudioState::UNSUPPORTED)
                }
            }
            Some(BackendKind::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    match self.windows_audio.as_ref() {
                        Some(driver) => driver.node_audio_state(node),
                        None => Ok(NodeAudioState::UNSUPPORTED),
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = node;
                    Ok(NodeAudioState::UNSUPPORTED)
                }
            }
            Some(BackendKind::Demo) => Err(Self::unsupported(
                "demo resources are not part of the live composite",
            )),
            None => Err(Self::unsupported("node has an unknown backend namespace")),
        }
    }

    pub(super) fn route_node_capabilities(&self, node: NodeId) -> NodeCapabilities {
        match backend_for_node(node) {
            Some(BackendKind::AlsaMidi) | Some(BackendKind::WindowsMidi) => NodeCapabilities::NONE,
            Some(BackendKind::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    match self.pipewire.as_ref() {
                        Some(driver) => driver.node_capabilities(node),
                        None => NodeCapabilities::NONE,
                    }
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                {
                    let _ = node;
                    NodeCapabilities::NONE
                }
            }
            Some(BackendKind::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    match self.windows_audio.as_ref() {
                        Some(driver) => driver.node_capabilities(node),
                        None => NodeCapabilities::NONE,
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = node;
                    NodeCapabilities::NONE
                }
            }
            Some(BackendKind::Demo) | None => NodeCapabilities::NONE,
        }
    }

    pub(super) fn route_set_node_mute(&mut self, node: NodeId, muted: bool) -> BackendResult<()> {
        match backend_for_node(node) {
            Some(BackendKind::AlsaMidi) => Err(Self::unsupported(
                "ALSA MIDI nodes do not expose audio mute",
            )),
            Some(BackendKind::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    self.pipewire_mut()?.set_node_mute(node, muted)
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                {
                    let _ = (node, muted);
                    Err(Self::unsupported("PipeWire backend is disabled"))
                }
            }
            Some(BackendKind::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    self.windows_audio
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                        .set_node_mute(node, muted)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = (node, muted);
                    Err(Self::unsupported("Windows audio backend is unavailable"))
                }
            }
            Some(BackendKind::WindowsMidi) => Err(Self::unsupported(
                "Windows MIDI nodes do not expose audio mute",
            )),
            Some(BackendKind::Demo) => Err(Self::unsupported(
                "demo resources are not part of the live composite",
            )),
            None => Err(Self::unsupported("node has an unknown backend namespace")),
        }
    }

    pub(super) fn route_set_node_volume(&mut self, node: NodeId, volume: f32) -> BackendResult<()> {
        match backend_for_node(node) {
            Some(BackendKind::AlsaMidi) => Err(Self::unsupported(
                "ALSA MIDI nodes do not expose audio volume",
            )),
            Some(BackendKind::PipeWire) => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                {
                    self.pipewire_mut()?.set_node_volume(node, volume)
                }
                #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
                {
                    let _ = (node, volume);
                    Err(Self::unsupported("PipeWire backend is disabled"))
                }
            }
            Some(BackendKind::WindowsAudio) => {
                #[cfg(target_os = "windows")]
                {
                    self.windows_audio
                        .as_mut()
                        .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                        .set_node_volume(node, volume)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = (node, volume);
                    Err(Self::unsupported("Windows audio backend is unavailable"))
                }
            }
            Some(BackendKind::WindowsMidi) => Err(Self::unsupported(
                "Windows MIDI nodes do not expose audio volume",
            )),
            Some(BackendKind::Demo) => Err(Self::unsupported(
                "demo resources are not part of the live composite",
            )),
            None => Err(Self::unsupported("node has an unknown backend namespace")),
        }
    }
}
