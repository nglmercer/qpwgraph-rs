//! COM notification sinks.
//!
//! Core Audio calls these back on its own threads, so they may not touch the
//! worker's state directly. Each one records what changed and lets the
//! worker pick it up on its next pass.

use super::*;

pub(super) fn mark_session_endpoint_dirty(
    dirty: &Arc<AtomicBool>,
    endpoints: &Arc<Mutex<BTreeSet<String>>>,
    endpoint_id: &str,
) {
    if let Ok(mut endpoints) = endpoints.lock() {
        endpoints.insert(endpoint_id.to_owned());
    }
    dirty.store(true, Ordering::Release);
    // A process can exit and its PID can be reused without any endpoint
    // topology event. Invalidate capability probes on session churn as well.
    ProcessLoopbackSource::clear_capability_cache();
}

pub(super) fn take_session_dirty_endpoints(
    endpoints: &Arc<Mutex<BTreeSet<String>>>,
) -> BTreeSet<String> {
    endpoints
        .lock()
        .map(|mut endpoints| std::mem::take(&mut *endpoints))
        .unwrap_or_default()
}

#[windows::core::implement(Audio::IMMNotificationClient)]
pub(super) struct EndpointNotificationClient {
    pub(super) dirty: Arc<AtomicBool>,
    pub(super) topology_dirty: Arc<AtomicBool>,
    #[cfg(feature = "relay")]
    pub(super) default_generation: Arc<AtomicU64>,
}

impl EndpointNotificationClient {
    pub(super) fn mark_topology_dirty(&self) {
        self.topology_dirty.store(true, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        // Device/service changes can invalidate a cached process-loopback
        // activation result, so the next capability query must be live.
        ProcessLoopbackSource::clear_capability_cache();
    }
}

impl Audio::IMMNotificationClient_Impl for EndpointNotificationClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: Audio::DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.mark_topology_dirty();
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.mark_topology_dirty();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.mark_topology_dirty();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _flow: Audio::EDataFlow,
        _role: Audio::ERole,
        _device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        #[cfg(feature = "relay")]
        self.default_generation.fetch_add(1, Ordering::AcqRel);
        self.mark_topology_dirty();
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        self.mark_topology_dirty();
        Ok(())
    }
}

#[windows::core::implement(Audio::IAudioSessionNotification)]
pub(super) struct SessionNotificationClient {
    pub(super) dirty: Arc<AtomicBool>,
    pub(super) endpoint_id: String,
    pub(super) session_dirty_endpoints: Arc<Mutex<BTreeSet<String>>>,
}

impl Audio::IAudioSessionNotification_Impl for SessionNotificationClient_Impl {
    fn OnSessionCreated(
        &self,
        _new_session: windows::core::Ref<Audio::IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        mark_session_endpoint_dirty(
            &self.dirty,
            &self.session_dirty_endpoints,
            &self.endpoint_id,
        );
        Ok(())
    }
}

/// Applies a volume/mute change to the shared state map.
///
/// Both callbacks receive the new values in the payload, so nothing has to be
/// read back over COM and the graph never needs rebuilding for a fader move.
pub(super) fn apply_state_change(
    states: &AudioStateMap,
    node_id: NodeId,
    volume: f32,
    muted: bool,
) {
    let Ok(mut states) = states.lock() else {
        return;
    };
    let state = states.entry(node_id).or_default();
    // A valid callback payload is itself a read. This also promotes a node
    // whose initial activation/readback failed, so it does not stay unknown
    // until the next topology rebuild.
    state.volume = Some(volume.clamp(0.0, 1.0));
    state.volume_readable = true;
    state.muted = Some(muted);
    state.mute_readable = true;
}

#[windows::core::implement(Audio::Endpoints::IAudioEndpointVolumeCallback)]
pub(super) struct EndpointVolumeCallback {
    pub(super) node_id: NodeId,
    pub(super) states: AudioStateMap,
}

impl Audio::Endpoints::IAudioEndpointVolumeCallback_Impl for EndpointVolumeCallback_Impl {
    fn OnNotify(
        &self,
        notify: *mut Audio::AUDIO_VOLUME_NOTIFICATION_DATA,
    ) -> windows::core::Result<()> {
        if notify.is_null() {
            return Ok(());
        }
        // Fields are read through the raw pointer: the struct is variable
        // length (a trailing channel-volume array) so it is never referenced
        // as a whole value.
        let (volume, muted) = unsafe {
            (
                std::ptr::addr_of!((*notify).fMasterVolume).read_unaligned(),
                std::ptr::addr_of!((*notify).bMuted).read_unaligned(),
            )
        };
        apply_state_change(&self.states, self.node_id, volume, muted.as_bool());
        Ok(())
    }
}

#[windows::core::implement(Audio::IAudioSessionEvents)]
pub(super) struct SessionEventsClient {
    pub(super) dirty: Arc<AtomicBool>,
    pub(super) endpoint_id: String,
    pub(super) session_dirty_endpoints: Arc<Mutex<BTreeSet<String>>>,
    pub(super) node_id: NodeId,
    pub(super) states: AudioStateMap,
}

impl Audio::IAudioSessionEvents_Impl for SessionEventsClient_Impl {
    fn OnDisplayNameChanged(
        &self,
        _new_display_name: &PCWSTR,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        self.mark_session_endpoint_dirty();
        Ok(())
    }

    fn OnIconPathChanged(
        &self,
        _new_icon_path: &PCWSTR,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    /// The payload already carries the new values, so this updates the shared
    /// state directly. It deliberately does not mark the topology dirty: a
    /// session''s volume changing does not change the graph, and forcing a full
    /// endpoint and session re-enumeration for every fader tick was the bulk of
    /// the refresh churn.
    fn OnSimpleVolumeChanged(
        &self,
        new_volume: f32,
        new_mute: BOOL,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        apply_state_change(&self.states, self.node_id, new_volume, new_mute.as_bool());
        Ok(())
    }

    /// Per-channel volume does not change the master scalar this driver
    /// reports, and it never changes the graph, so it is ignored rather than
    /// triggering a rebuild.
    fn OnChannelVolumeChanged(
        &self,
        _channel_count: u32,
        _new_channel_volume_array: *const f32,
        _changed_channel: u32,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _new_grouping_param: *const GUID,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, _new_state: Audio::AudioSessionState) -> windows::core::Result<()> {
        self.mark_session_endpoint_dirty();
        Ok(())
    }

    fn OnSessionDisconnected(
        &self,
        _disconnect_reason: Audio::AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        self.mark_session_endpoint_dirty();
        Ok(())
    }
}

impl SessionEventsClient {
    pub(super) fn mark_session_endpoint_dirty(&self) {
        mark_session_endpoint_dirty(
            &self.dirty,
            &self.session_dirty_endpoints,
            &self.endpoint_id,
        );
    }
}
