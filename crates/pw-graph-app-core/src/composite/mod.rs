//! One merged graph over the platform's native backends, with disjoint ID
//! namespaces.
//!
//! This module owns the struct, how it is opened, and the `GraphDriver`
//! surface. That surface is deliberately thin: each method names the
//! submodule that answers it.
//!
//! | Module | Answers |
//! | --- | --- |
//! | [`routing`] | which child owns a node, port or link, and the mutation |
//! | [`refresh`] | the merged graph and each child's refresh clock |
//! | [`effects`] | effect hosting |
//! | [`relay`] | the relay engine on whichever child provides it |
//! | [`platform`] | accessors that exist on only one platform |

use super::*;
use std::collections::BTreeMap;

mod effects;
mod platform;
pub(crate) mod refresh;
mod relay;
mod routing;

pub use routing::{route_for_ports, CompositeRoute};

#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", any(feature = "pipewire", feature = "alsa"))
))]
use refresh::RefreshSchedule;

/// PipeWire + ALSA MIDI graph with disjoint ID namespaces.
#[derive(Default)]
pub struct CompositeDriver {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    pub pipewire: Option<PipewireDriver>,
    #[cfg(all(target_os = "linux", feature = "alsa"))]
    pub alsa: Option<AlsaMidiDriver>,
    #[cfg(target_os = "windows")]
    pub windows_audio: Option<WindowsAudioDriver>,
    #[cfg(target_os = "windows")]
    pub windows_midi: Option<WindowsMidiDriver>,
    graph: Graph,
    recorder_owners: BTreeMap<pw_graph_backend::RecorderId, CompositeRoute>,
    #[cfg(any(
        target_os = "windows",
        all(target_os = "linux", any(feature = "pipewire", feature = "alsa"))
    ))]
    refresh_schedule: RefreshSchedule,
}

impl CompositeDriver {
    /// Construct a live composite and retain any per-backend startup errors.
    /// This is intentionally the only place where the two native drivers are
    /// opened for the desktop application.
    #[allow(unused_mut)]
    pub fn open(no_midi: bool) -> (Self, BackendAvailability) {
        let _ = no_midi;
        let mut composite = Self::default();
        let mut availability = BackendAvailability::default();

        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        match PipewireDriver::new() {
            Ok(driver) => {
                composite.pipewire = Some(driver);
                availability.pipewire = true;
            }
            Err(error) => availability.failures.push(error.to_string()),
        }

        #[cfg(all(target_os = "linux", feature = "alsa"))]
        if !no_midi {
            match AlsaMidiDriver::new() {
                Ok(driver) => {
                    composite.alsa = Some(driver);
                    availability.alsa = true;
                }
                Err(error) => availability.failures.push(error.to_string()),
            }
        }

        #[cfg(target_os = "windows")]
        match WindowsAudioDriver::new() {
            Ok(driver) => {
                composite.windows_audio = Some(driver);
                availability.windows_audio = true;
            }
            Err(error) => availability.failures.push(error.to_string()),
        }

        #[cfg(target_os = "windows")]
        if !no_midi {
            match WindowsMidiDriver::new() {
                Ok(driver) => {
                    composite.windows_midi = Some(driver);
                    availability.windows_midi = true;
                }
                Err(error) => availability.failures.push(error.to_string()),
            }
        }

        (composite, availability)
    }

    /// Build a composite from already-created children.  This is useful for
    /// deterministic integration tests and keeps construction independent of
    /// the windowing toolkit.
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    pub fn with_pipewire(driver: PipewireDriver) -> Self {
        Self {
            pipewire: Some(driver),
            #[cfg(all(target_os = "linux", feature = "alsa"))]
            alsa: None,
            #[cfg(target_os = "windows")]
            windows_audio: None,
            refresh_schedule: RefreshSchedule::default(),
            graph: Graph::default(),
            recorder_owners: BTreeMap::new(),
        }
    }

    #[cfg(target_os = "windows")]
    pub fn with_windows_audio(driver: WindowsAudioDriver) -> Self {
        Self {
            windows_audio: Some(driver),
            windows_midi: None,
            refresh_schedule: RefreshSchedule::default(),
            graph: Graph::default(),
            recorder_owners: BTreeMap::new(),
        }
    }

    #[cfg(target_os = "windows")]
    pub fn with_windows_midi(driver: WindowsMidiDriver) -> Self {
        Self {
            windows_audio: None,
            windows_midi: Some(driver),
            refresh_schedule: RefreshSchedule::default(),
            graph: Graph::default(),
            recorder_owners: BTreeMap::new(),
        }
    }

    pub fn has_pipewire(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire.is_some()
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            false
        }
    }

    pub fn has_alsa(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "alsa"))]
        {
            self.alsa.is_some()
        }
        #[cfg(not(all(target_os = "linux", feature = "alsa")))]
        {
            false
        }
    }

    pub fn has_windows_audio(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.windows_audio.is_some()
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }
}

impl GraphDriver for CompositeDriver {
    #[allow(unused_mut)]
    fn capabilities(&self) -> BackendCapabilities {
        let mut capabilities = BackendCapabilities::default();
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Some(driver) = self.pipewire.as_ref() {
            capabilities = capabilities.union(driver.capabilities());
        }
        #[cfg(all(target_os = "linux", feature = "alsa"))]
        if let Some(driver) = self.alsa.as_ref() {
            capabilities = capabilities.union(driver.capabilities());
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_audio.as_ref() {
            capabilities = capabilities.union(driver.capabilities());
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_midi.as_ref() {
            capabilities = capabilities.union(driver.capabilities());
        }
        capabilities
    }

    fn refresh(&mut self) -> BackendResult<Vec<Node>> {
        self.refresh_all()
    }

    fn refresh_if_needed(&mut self) -> BackendResult<Vec<Node>> {
        self.refresh_due_children()
    }

    fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        self.route_connect(src, dst)
    }

    fn disconnect(&mut self, link: LinkId) -> BackendResult<Link> {
        self.route_disconnect(link)
    }

    fn is_link_mutable(&self, link: LinkId) -> bool {
        self.route_is_link_mutable(link)
    }

    fn connection_support(
        &self,
        output: PortId,
        input: PortId,
    ) -> pw_graph_backend::ConnectionSupport {
        match route_for_ports(output, input) {
            Ok(CompositeRoute::PipeWire) =>
            {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                if let Some(driver) = self.pipewire.as_ref() {
                    return driver.connection_support(output, input);
                }
            }
            Ok(CompositeRoute::WindowsAudio) =>
            {
                #[cfg(target_os = "windows")]
                if let Some(driver) = self.windows_audio.as_ref() {
                    return driver.connection_support(output, input);
                }
            }
            _ => {}
        }
        pw_graph_backend::ConnectionSupport::Unsupported
    }

    fn node_supports_routing(&self, node: NodeId) -> bool {
        let Some(backend) = backend_for_node(node) else {
            return false;
        };
        self.child_node_supports_routing(backend, node)
    }

    fn set_node_position(&mut self, node: NodeId, position: [f32; 2]) -> BackendResult<()> {
        self.route_set_node_position(node, position)
    }

    fn node_audio_state(&self, node: NodeId) -> BackendResult<NodeAudioState> {
        self.route_node_audio_state(node)
    }

    fn node_capabilities(&self, node: NodeId) -> NodeCapabilities {
        self.route_node_capabilities(node)
    }

    fn set_node_mute(&mut self, node: NodeId, muted: bool) -> BackendResult<()> {
        self.route_set_node_mute(node, muted)
    }

    fn set_node_volume(&mut self, node: NodeId, volume: f32) -> BackendResult<()> {
        self.route_set_node_volume(node, volume)
    }

    fn reports_graph_changes(&self) -> bool {
        self.children_report_graph_changes()
    }

    fn graph(&self) -> &Graph {
        self.merged_graph()
    }

    fn graph_dirty(&self) -> bool {
        self.any_child_dirty()
    }

    fn is_node_type(&self, node_type: NodeType) -> bool {
        let _ = node_type;
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if self
            .pipewire
            .as_ref()
            .is_some_and(|driver| driver.is_node_type(node_type))
        {
            return true;
        }
        #[cfg(all(target_os = "linux", feature = "alsa"))]
        if self
            .alsa
            .as_ref()
            .is_some_and(|driver| driver.is_node_type(node_type))
        {
            return true;
        }
        #[cfg(target_os = "windows")]
        if self
            .windows_audio
            .as_ref()
            .is_some_and(|driver| driver.is_node_type(node_type))
        {
            return true;
        }
        #[cfg(target_os = "windows")]
        if self
            .windows_midi
            .as_ref()
            .is_some_and(|driver| driver.is_node_type(node_type))
        {
            return true;
        }
        false
    }

    fn is_port_type(&self, port_type: PortType) -> bool {
        let _ = port_type;
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if self
            .pipewire
            .as_ref()
            .is_some_and(|driver| driver.is_port_type(port_type))
        {
            return true;
        }
        #[cfg(all(target_os = "linux", feature = "alsa"))]
        if self
            .alsa
            .as_ref()
            .is_some_and(|driver| driver.is_port_type(port_type))
        {
            return true;
        }
        #[cfg(target_os = "windows")]
        if self
            .windows_audio
            .as_ref()
            .is_some_and(|driver| driver.is_port_type(port_type))
        {
            return true;
        }
        #[cfg(target_os = "windows")]
        if self
            .windows_midi
            .as_ref()
            .is_some_and(|driver| driver.is_port_type(port_type))
        {
            return true;
        }
        false
    }

    fn audio_meters(&mut self) -> BackendResult<Vec<pw_graph_backend::AudioMeter>> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Some(driver) = self.pipewire.as_mut() {
            return driver.audio_meters();
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_audio.as_mut() {
            return driver.audio_meters();
        }
        Ok(Vec::new())
    }

    fn set_meter_policy(&mut self, policy: MeterPolicy) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Some(driver) = self.pipewire.as_mut() {
            return driver.set_meter_policy(policy);
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_audio.as_mut() {
            return driver.set_meter_policy(policy);
        }
        let _ = policy;
        Ok(())
    }

    fn request_meters(&mut self, nodes: &BTreeSet<NodeId>) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Some(driver) = self.pipewire.as_mut() {
            return driver.request_meters(nodes);
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_audio.as_mut() {
            return driver.request_meters(nodes);
        }
        let _ = nodes;
        Ok(())
    }

    fn reset_audio_config(&mut self) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Some(driver) = self.pipewire.as_mut() {
            return driver.reset_audio_config();
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_audio.as_mut() {
            return driver.reset_audio_config();
        }
        Ok(())
    }
}

impl RecorderDriver for CompositeDriver {
    fn supports_recorders(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if self
            .pipewire
            .as_ref()
            .is_some_and(RecorderDriver::supports_recorders)
        {
            return true;
        }
        #[cfg(target_os = "windows")]
        if self
            .windows_audio
            .as_ref()
            .is_some_and(RecorderDriver::supports_recorders)
        {
            return true;
        }
        false
    }

    fn create_recorder(
        &mut self,
        request: pw_graph_backend::RecorderCreateRequest,
    ) -> BackendResult<pw_graph_backend::RecorderInstance> {
        let _ = &request;
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Some(driver) = self.pipewire.as_mut() {
            let instance = driver.create_recorder(request)?;
            self.recorder_owners
                .insert(instance.id, CompositeRoute::PipeWire);
            self.rebuild_merged_graph()?;
            return Ok(instance);
        }
        #[cfg(target_os = "windows")]
        if let Some(driver) = self.windows_audio.as_mut() {
            let instance = driver.create_recorder(request)?;
            self.recorder_owners
                .insert(instance.id, CompositeRoute::WindowsAudio);
            self.rebuild_merged_graph()?;
            return Ok(instance);
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn remove_recorder(&mut self, id: pw_graph_backend::RecorderId) -> BackendResult<()> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                self.pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .remove_recorder(id)?;
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                self.windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .remove_recorder(id)?;
            }
            _ => {
                return Err(BackendError::Unsupported(
                    "recorder backend is unavailable".into(),
                ));
            }
        }
        self.recorder_owners.remove(&id);
        self.rebuild_merged_graph()?;
        Ok(())
    }

    fn start_recording(&mut self, id: pw_graph_backend::RecorderId) -> BackendResult<()> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .start_recording(id);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .start_recording(id);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn request_stop_recording(&mut self, id: pw_graph_backend::RecorderId) -> BackendResult<()> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .request_stop_recording(id);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .request_stop_recording(id);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn stop_recording(
        &mut self,
        id: pw_graph_backend::RecorderId,
    ) -> BackendResult<pw_graph_backend::RecorderResult> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .stop_recording(id);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .stop_recording(id);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn save_recording(
        &mut self,
        id: pw_graph_backend::RecorderId,
        destination: &std::path::Path,
    ) -> BackendResult<std::path::PathBuf> {
        let _ = destination;
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .save_recording(id, destination);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .save_recording(id, destination);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn recorder_status(
        &self,
        id: pw_graph_backend::RecorderId,
    ) -> BackendResult<pw_graph_backend::RecorderStatus> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_ref()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .recorder_status(id);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_ref()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .recorder_status(id);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn poll_recording(
        &mut self,
        id: pw_graph_backend::RecorderId,
    ) -> BackendResult<Option<pw_graph_backend::RecorderResult>> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .poll_recording(id);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .poll_recording(id);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }

    fn discard_recording(&mut self, id: pw_graph_backend::RecorderId) -> BackendResult<()> {
        let owner = self
            .recorder_owners
            .get(&id)
            .copied()
            .ok_or_else(|| BackendError::Native(format!("unknown recorder {id}")))?;
        match owner {
            CompositeRoute::PipeWire => {
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                return self
                    .pipewire
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("PipeWire backend is unavailable".into())
                    })?
                    .discard_recording(id);
            }
            CompositeRoute::WindowsAudio => {
                #[cfg(target_os = "windows")]
                return self
                    .windows_audio
                    .as_mut()
                    .ok_or_else(|| {
                        BackendError::Unsupported("Windows audio backend is unavailable".into())
                    })?
                    .discard_recording(id);
            }
            _ => {}
        }
        Err(BackendError::Unsupported(
            "recorder backend is unavailable".into(),
        ))
    }
}
