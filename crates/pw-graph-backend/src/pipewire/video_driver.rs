//! Video filter nodes, screen capture, and previews on the PipeWire driver.
//!
//! Each video filter is one synthetic graph node (video in + video out) backed
//! by a [`VideoBridge`]: an optional capture stream retargeted at the upstream
//! node, a worker thread, and an output stream publishing processed frames.
//! Synthetic links are projected into the shared graph on every rebuild:
//!
//! ```text
//! real video out ---> bridge in    = capture retarget (no daemon link)
//! bridge out    ---> real video in = daemon link from the output stream
//! bridge A out  ---> bridge B in   = B captures A's output stream node
//! ```
//!
//! Screen capture and virtual displays are portal sessions whose PipeWire
//! stream nodes are ordinary graph nodes; the driver only tracks their
//! identity and attaches a preview tap.

use super::video::bridge::VideoBridge;
use super::video::capture::{create_video_capture_locked, VideoCaptureShared};
use super::video::format::SUPPORTED_PIXEL_FORMATS;
use super::video::output::create_video_output_locked;
use super::*;
use crate::linux::screencast::PortalConnector;
#[cfg(not(feature = "screencast"))]
use crate::linux::screencast::StubPortalConnector;
use crate::video::{
    ScreenCastRequest, ScreenCastSource, ScreenCastStatus, VideoDriver, VideoFilterInstance,
    VideoFilterInstanceId, VideoFilterRequest, VideoNodeInfo, VideoNodeState,
    VirtualDisplayRequest, VirtualDisplayStatus,
};
use pw_graph_video::filters;
use pw_graph_video::preview::{PreviewState, VideoPreview};

/// Synthetic link projection: how a user-visible link maps onto PipeWire.
#[derive(Clone, Debug)]
pub(super) struct VideoLinkProjection {
    pub output: PortId,
    pub input: PortId,
    /// Daemon link id for `bridge out -> real in` projections.
    pub real_link: Option<LinkId>,
    /// Bridge whose capture stream was retargeted (`-> bridge in`).
    pub capture_bridge: Option<VideoFilterInstanceId>,
}

/// Portal connector for this build: live ashpd when the `screencast` feature
/// is on, otherwise a stub that reports unavailability cleanly.
pub(super) fn new_portal_connector() -> Box<dyn PortalConnector> {
    #[cfg(feature = "screencast")]
    {
        Box::new(crate::linux::screencast::live::AshpdPortalConnector::new())
    }
    #[cfg(not(feature = "screencast"))]
    {
        Box::new(StubPortalConnector::new())
    }
}

pub(super) fn is_video_synthetic_id(id: u64) -> bool {
    decode_backend_namespace(id) == BackendNamespace::PipeWire
        && decode_backend_local_id(id) >= VIDEO_SYNTHETIC_BASE
}

/// Deterministic 64-bit FNV-1a hash for synthetic node serials. Stable across
/// restarts so patchbay serial matching works for filter nodes.
fn video_serial(instance_id: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in instance_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn video_node_name(filter_id: &str, instance_id: &str) -> String {
    format!("qpwgraph-video-{filter_id}-{instance_id}")
}

fn video_capture_stream_name(instance_id: &str) -> String {
    format!("{VIDEO_STREAM_PREFIX}in-{instance_id}")
}

fn video_output_stream_name(instance_id: &str) -> String {
    format!("{VIDEO_STREAM_PREFIX}out-{instance_id}")
}

const SCREENCAST_PREVIEW_STREAM: &str = "qpwgraph-video-preview";

impl PipewireDriver {
    fn alloc_video_id(&mut self) -> u64 {
        let local = VIDEO_SYNTHETIC_BASE | (self.video_id_seq & 0x0FFF_FFFF);
        self.video_id_seq = self.video_id_seq.wrapping_add(1);
        graph_id(local)
    }

    /// Which bridge (if any) owns `port`, and whether it is the input side.
    fn video_bridge_for_port(&self, port: PortId) -> Option<(&VideoFilterInstanceId, bool)> {
        self.video_bridges.iter().find_map(|(id, bridge)| {
            if bridge.input_port() == port {
                Some((id, true))
            } else if bridge.output_port() == port {
                Some((id, false))
            } else {
                None
            }
        })
    }

    /// `target.object` for a capture stream: serial preferred, name fallback.
    fn target_for_node(&self, node_id: NodeId) -> BackendResult<String> {
        let node = self
            .graph
            .node(node_id)
            .ok_or(GraphError::MissingNode(node_id))?;
        if let Some(serial) = node.serial {
            return Ok(serial.to_string());
        }
        if node.name.trim().is_empty() {
            return Err(BackendError::Native(
                "capture target node has no identity".into(),
            ));
        }
        Ok(node.name.clone())
    }

    /// Real source endpoint (graph port id plus native node id) of a
    /// bridge's output stream node, if negotiated. Helper streams are
    /// filtered out of the rendered graph, so this scans the registry
    /// state rather than the graph.
    fn bridge_output_source_port(&self, instance_id: &str) -> Option<(PortId, u32)> {
        let stream_name = video_output_stream_name(instance_id);
        let state = self.state.lock().unwrap();
        let (node_id, media_class) = state
            .nodes
            .iter()
            .find(|(_, record)| record.name == stream_name)
            .map(|(id, record)| (*id, record.media_class.clone()))?;
        state
            .ports
            .iter()
            .find(|(_, record)| {
                record.node_id == node_id
                    && record.direction.is_source()
                    && super::registry::classify_port_type(
                        &record.media_type,
                        Some(media_class.as_str()),
                    ) == PortType::Video
            })
            .map(|(id, _)| (PortId(graph_id(*id as u64)), node_id))
    }

    fn create_video_filter_locked(
        &mut self,
        request: VideoFilterRequest,
    ) -> BackendResult<VideoFilterInstance> {
        if request.instance_id.trim().is_empty() {
            return Err(BackendError::Native(
                "video filter instance id is empty".into(),
            ));
        }
        if self.video_bridges.contains_key(&request.instance_id) {
            return Err(BackendError::Native(format!(
                "video filter instance {} already exists",
                request.instance_id
            )));
        }
        // Validate the filter id and parameters before touching the graph.
        let processor =
            filters::create_filter(&request.filter_id, &request.params).map_err(|error| {
                BackendError::Native(format!(
                    "unknown video filter {}: {error}",
                    request.filter_id
                ))
            })?;
        let node_id = NodeId(self.alloc_video_id());
        let input_port = PortId(self.alloc_video_id());
        let output_port = PortId(self.alloc_video_id());
        let instance = VideoFilterInstance {
            instance_id: request.instance_id.clone(),
            node_id,
            input_port,
            output_port,
            filter_id: request.filter_id.clone(),
        };
        let bridge = VideoBridge::new(instance.clone(), request.filter_id.clone(), request.params);
        bridge.ensure_worker(processor);
        self.positions.insert(node_id, request.position);
        self.video_bridges
            .insert(request.instance_id.clone(), bridge);
        self.rebuild_graph_locked()?;
        Ok(instance)
    }

    fn remove_video_filter_locked(&mut self, instance_id: &str) -> BackendResult<()> {
        let bridge = self.video_bridges.get(instance_id).ok_or_else(|| {
            BackendError::Native(format!("unknown video filter instance {instance_id}"))
        })?;
        let (input_port, output_port) = (bridge.input_port(), bridge.output_port());
        // Destroy daemon links owned by this bridge's projections first.
        let real_links: Vec<LinkId> = self
            .video_links
            .values()
            .filter(|projection| projection.output == output_port || projection.input == input_port)
            .filter_map(|projection| projection.real_link)
            .collect();
        for real_link in real_links {
            // A vanished daemon link is fine; the mapping is going away
            // regardless.
            let _ = self.destroy_daemon_link_locked(native_link_id(real_link));
        }
        self.video_links.retain(|_, projection| {
            projection.output != output_port && projection.input != input_port
        });
        let node_id = self
            .video_bridges
            .get(instance_id)
            .map(|bridge| bridge.node_id());
        self.video_bridges.remove(instance_id);
        if let Some(node_id) = node_id {
            self.positions.remove(&node_id);
        }
        self.rebuild_graph_locked()?;
        Ok(())
    }

    pub(super) fn video_connect_locked(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        let output = self
            .graph
            .port(src)
            .cloned()
            .ok_or(GraphError::MissingPort(src))?;
        let input = self
            .graph
            .port(dst)
            .cloned()
            .ok_or(GraphError::MissingPort(dst))?;
        if !output.direction.is_source() {
            return Err(GraphError::NotSource(src).into());
        }
        if !input.direction.is_sink() {
            return Err(GraphError::NotSink(dst).into());
        }
        if output.port_type != input.port_type
            && output.port_type != PortType::Unknown
            && input.port_type != PortType::Unknown
        {
            return Err(GraphError::IncompatiblePorts(src, dst).into());
        }
        if self
            .video_links
            .values()
            .any(|projection| projection.output == src && projection.input == dst)
            || self
                .graph
                .links
                .values()
                .any(|link| link.output_port == src && link.input_port == dst)
        {
            return Err(GraphError::DuplicateConnection(src, dst).into());
        }
        let src_bridge = self.video_bridge_for_port(src).map(|(id, _)| id.clone());
        let dst_bridge = self.video_bridge_for_port(dst).map(|(id, _)| id.clone());
        // A bridge output feeding its own input is a feedback loop.
        if src_bridge.is_some() && src_bridge == dst_bridge {
            return Err(BackendError::Native(
                "a video filter cannot feed its own input".into(),
            ));
        }

        // Case `-> bridge in`: retarget the destination capture stream.
        if let Some(dst_id) = dst_bridge {
            let target = if let Some(src_id) = &src_bridge {
                if self.bridge_output_source_port(src_id).is_none() {
                    return Err(BackendError::Native(format!(
                        "upstream filter {src_id} has no output stream yet; link its input and wait for negotiation"
                    )));
                }
                video_output_stream_name(src_id)
            } else {
                self.target_for_node(output.node_id)?
            };
            self.retarget_bridge_capture_locked(&dst_id, &target)?;
            let link_id = LinkId(self.alloc_video_id());
            self.video_links.insert(
                link_id,
                VideoLinkProjection {
                    output: src,
                    input: dst,
                    real_link: None,
                    capture_bridge: Some(dst_id),
                },
            );
            self.roundtrip_locked()?;
            self.rebuild_graph_locked()?;
            return self
                .graph
                .link(link_id)
                .cloned()
                .ok_or_else(|| BackendError::Native("video link projection failed".into()));
        }

        // Case `bridge out -> real in`: daemon link from the output stream.
        // The output port is a filtered helper port, so the daemon link is
        // created directly from native ids instead of through the
        // graph-validated `connect_locked`.
        if let Some(src_id) = src_bridge {
            let (real_src, real_node) = self.bridge_output_source_port(&src_id).ok_or_else(|| {
                BackendError::Native(format!(
                    "filter {src_id} has no negotiated output yet; link its input and wait for negotiation"
                ))
            })?;
            let link_id = LinkId(self.alloc_video_id());
            self.video_links.insert(
                link_id,
                VideoLinkProjection {
                    output: src,
                    input: dst,
                    real_link: None,
                    capture_bridge: None,
                },
            );
            let native_out = native_port_id(real_src);
            let native_in_node = native_node_id(input.node_id);
            let native_in = native_port_id(dst);
            // Adopt a session-manager link when one already connects these
            // endpoints (consumers with `target.object` auto-link); only
            // create when nothing exists. Duplicate creation is rejected
            // by the daemon.
            let existing = self
                .state
                .lock()
                .unwrap()
                .links
                .iter()
                .find(|(_, link)| link.output_port == native_out && link.input_port == native_in)
                .map(|(id, _)| *id);
            let created = match existing {
                Some(native_link) => Ok(native_link),
                None => {
                    self.create_daemon_link_locked(real_node, native_out, native_in_node, native_in)
                }
            };
            match created {
                Ok(native_link) => {
                    if let Some(projection) = self.video_links.get_mut(&link_id) {
                        projection.real_link = Some(LinkId(graph_id(native_link as u64)));
                    }
                    self.rebuild_graph_locked()?;
                    return self.graph.link(link_id).cloned().ok_or_else(|| {
                        BackendError::Native("video link projection failed".into())
                    });
                }
                Err(error) => {
                    self.video_links.remove(&link_id);
                    return Err(error);
                }
            }
        }

        Err(BackendError::Native(
            "video connect reached without a filter endpoint".into(),
        ))
    }

    /// Destroy and recreate a bridge's capture stream aimed at `target`.
    fn retarget_bridge_capture_locked(
        &mut self,
        instance_id: &str,
        target: &str,
    ) -> BackendResult<()> {
        let core = self.core()?.clone();
        let bridge = self.video_bridges.get(instance_id).ok_or_else(|| {
            BackendError::Native(format!("unknown video filter instance {instance_id}"))
        })?;
        let shared =
            VideoCaptureShared::with_queue(bridge.queue.clone(), bridge.diagnostics.clone());
        let stream_name = video_capture_stream_name(instance_id);
        // Drop the old stream before creating the new one so only one
        // capture per bridge ever exists.
        if let Some(bridge) = self.video_bridges.get_mut(instance_id) {
            bridge.capture = None;
        }
        let handle = create_video_capture_locked(
            &core,
            &stream_name,
            target,
            &SUPPORTED_PIXEL_FORMATS,
            &shared,
        )?;
        if let Some(bridge) = self.video_bridges.get_mut(instance_id) {
            bridge.capture = Some(handle);
            bridge.clear_failure();
        }
        Ok(())
    }

    pub(super) fn video_disconnect_locked(&mut self, link: LinkId) -> BackendResult<Link> {
        let projection = self
            .video_links
            .remove(&link)
            .ok_or(GraphError::MissingLink(link))?;
        if let Some(real_link) = projection.real_link {
            // Destroying a vanished daemon link is fine; the projection is
            // already gone. Direct destroy: helper links are not in the
            // graph, so the graph-validated `disconnect_locked` cannot
            // remove them.
            let _ = self.destroy_daemon_link_locked(native_link_id(real_link));
        }
        if let Some(bridge_id) = &projection.capture_bridge {
            if let Some(bridge) = self.video_bridges.get_mut(bridge_id) {
                bridge.capture = None;
                bridge.note_input_detached();
            }
        }
        // No daemon traffic happened for pure capture detachments, but the
        // stream nodes need a roundtrip to disappear from the registry.
        self.roundtrip_locked()?;
        self.rebuild_graph_locked()?;
        Ok(Link {
            id: link,
            output_port: projection.output,
            input_port: projection.input,
        })
    }

    /// Re-project synthetic filter nodes and links into a freshly built
    /// graph. Called at the end of `build_graph_from_state`; drops
    /// projections whose real endpoints vanished. Daemon-link liveness comes
    /// from the registry because helper-stream ports are filtered out of the
    /// graph and their daemon links never appear as graph links.
    pub(super) fn inject_video_nodes(
        &mut self,
        graph: &mut Graph,
        daemon_links: &std::collections::BTreeMap<u32, super::registry::LinkRecord>,
    ) {
        for bridge in self.video_bridges.values() {
            let instance = &bridge.instance;
            let mut node = Node::new(
                instance.node_id,
                video_node_name(&bridge.filter_id, &instance.instance_id),
                NodeType::Effect,
            );
            node = node.with_serial(video_serial(&instance.instance_id));
            node.position = self
                .positions
                .get(&instance.node_id)
                .copied()
                .unwrap_or([0.0, 0.0]);
            if graph.add_node(node).is_err() {
                // Synthetic id collided with a daemon id; skip the node
                // rather than corrupting the graph.
                continue;
            }
            let input = Port::new(
                instance.input_port,
                instance.node_id,
                "video_in",
                Direction::Sink,
                PortType::Video,
            );
            let output = Port::new(
                instance.output_port,
                instance.node_id,
                "video_out",
                Direction::Source,
                PortType::Video,
            );
            if graph.add_port(input).is_err() || graph.add_port(output).is_err() {
                graph.nodes.remove(&instance.node_id);
                continue;
            }
        }
        // Prune projections whose endpoints or daemon links are gone.
        let mut dropped_inputs: Vec<VideoFilterInstanceId> = Vec::new();
        self.video_links.retain(|_, projection| {
            let endpoints_live =
                graph.port(projection.output).is_some() && graph.port(projection.input).is_some();
            let real_live = projection
                .real_link
                .is_none_or(|real| daemon_links.contains_key(&native_link_id(real)));
            let live = endpoints_live && real_live;
            if !live {
                if let Some(bridge) = &projection.capture_bridge {
                    dropped_inputs.push(bridge.clone());
                }
            }
            live
        });
        for bridge_id in dropped_inputs {
            if let Some(bridge) = self.video_bridges.get(&bridge_id) {
                bridge.mark_bypassed("video source disappeared");
            }
        }
        for (link_id, projection) in &self.video_links {
            let _ = graph.insert_existing_link(Link {
                id: *link_id,
                output_port: projection.output,
                input_port: projection.input,
            });
        }
    }

    /// Attach lazily-created output streams, reconcile bridge liveness, and
    /// track portal sessions. Called from `refresh` under the loop lock.
    pub(super) fn reconcile_video_locked(&mut self) -> BackendResult<()> {
        // Portal sessions: external closes and vanished stream nodes.
        self.screen_cast.poll();
        self.virtual_cast.poll();
        {
            let state = self.state.lock().unwrap();
            let visible: Vec<u32> = state.nodes.keys().copied().collect();
            self.screen_cast.reconcile_visible_streams(&visible);
            self.virtual_cast.reconcile_visible_streams(&visible);
            if let Some(node_id) = self.screen_cast.status().pipewire_node_id {
                let serial = state.nodes.get(&node_id).and_then(|record| record.serial);
                self.screen_cast.note_stream_serial(node_id, serial);
            }
        }
        self.reconcile_screencast_preview_locked()?;

        // Bridges: adopt negotiated capture specs and lazily attach outputs.
        let bridge_ids: Vec<String> = self.video_bridges.keys().cloned().collect();
        for bridge_id in bridge_ids {
            let capture_spec = self
                .video_bridges
                .get(&bridge_id)
                .and_then(|bridge| bridge.capture.as_ref())
                .and_then(|capture| capture.shared.spec.lock().ok().and_then(|spec| *spec));
            if let Some(spec) = capture_spec {
                if let Some(bridge) = self.video_bridges.get(&bridge_id) {
                    bridge.note_input_spec(&spec);
                }
                self.ensure_bridge_output_locked(&bridge_id, &spec)?;
            }
            if let Some(bridge) = self.video_bridges.get(&bridge_id) {
                bridge.reconcile_streams();
            }
        }
        Ok(())
    }

    /// Create (or recreate after renegotiation) a bridge's output stream for
    /// the negotiated input spec.
    fn ensure_bridge_output_locked(
        &mut self,
        bridge_id: &str,
        input_spec: &pw_graph_video::VideoSpec,
    ) -> BackendResult<()> {
        let desired = {
            let bridge = self.video_bridges.get(bridge_id).ok_or_else(|| {
                BackendError::Native(format!("unknown video filter instance {bridge_id}"))
            })?;
            let processor = filters::create_filter(&bridge.filter_id, &bridge.params)
                .map_err(|error| BackendError::Native(format!("video filter failed: {error}")))?;
            processor.output_spec(input_spec).map_err(|error| {
                BackendError::Native(format!("video filter output spec failed: {error}"))
            })?
        };
        let needs_rebuild = self
            .video_bridges
            .get(bridge_id)
            .map(|bridge| bridge.output_spec() != Some(desired) || bridge.output.is_none())
            .unwrap_or(false);
        if !needs_rebuild {
            return Ok(());
        }
        let core = self.core()?.clone();
        let source = self
            .video_bridges
            .get(bridge_id)
            .map(|bridge| bridge.preview.frame_slot())
            .ok_or_else(|| BackendError::Native("video bridge vanished".into()))?;
        let stream_name = video_output_stream_name(bridge_id);
        if let Some(bridge) = self.video_bridges.get_mut(bridge_id) {
            bridge.output = None;
        }
        match create_video_output_locked(&core, &stream_name, &desired, source) {
            Ok(handle) => {
                if let Some(bridge) = self.video_bridges.get_mut(bridge_id) {
                    bridge.output = Some(handle);
                    bridge.note_output_spec(&desired);
                    bridge.clear_failure();
                }
            }
            Err(error) => {
                if let Some(bridge) = self.video_bridges.get(bridge_id) {
                    bridge.mark_failed(&format!("output stream failed: {error}"));
                }
            }
        }
        Ok(())
    }

    /// Keep a preview tap on the active screen-cast stream node.
    fn reconcile_screencast_preview_locked(&mut self) -> BackendResult<()> {
        let active_node = self.screen_cast.status().pipewire_node_id;
        let Some(node_id) = active_node else {
            if self.screen_cast_preview.take().is_some() {
                self.screen_cast_preview_slot.restart();
            }
            return Ok(());
        };
        // Resolve the target from the live registry (serial preferred).
        let target = {
            let state = self.state.lock().unwrap();
            state.nodes.get(&node_id).map(|record| {
                record
                    .serial
                    .map(|serial| serial.to_string())
                    .unwrap_or_else(|| record.name.clone())
            })
        };
        let Some(target) = target else {
            return Ok(());
        };
        if self.screen_cast_preview.is_none() {
            let core = self.core()?.clone();
            let shared = VideoCaptureShared::with_queue(
                pw_graph_video::VideoQueue::new(DEFAULT_VIDEO_QUEUE_DEPTH),
                pw_graph_video::VideoDiagnostics::new(),
            );
            match create_video_capture_locked(
                &core,
                SCREENCAST_PREVIEW_STREAM,
                &target,
                &SUPPORTED_PIXEL_FORMATS,
                &shared,
            ) {
                Ok(handle) => {
                    self.screen_cast_preview_slot.restart();
                    self.screen_cast_preview = Some(handle);
                }
                Err(error) => {
                    self.screen_cast_preview_slot
                        .mark_error(format!("preview failed: {error}"));
                }
            }
        }
        // Mirror the newest raw frame into the preview slot when it advances.
        if let Some(preview) = &self.screen_cast_preview {
            if let Some(frame) = preview.shared.latest.latest() {
                if frame.sequence() != self.screen_cast_preview_sequence {
                    self.screen_cast_preview_sequence = frame.sequence();
                    self.screen_cast_preview_slot.publish(&frame);
                }
            }
            if !preview.shared.connected.load(Ordering::Relaxed) {
                // Not streaming (yet): keep the last frame, report idle.
                if self.screen_cast_preview_slot.latest().is_none() {
                    self.screen_cast_preview_slot.set_state(PreviewState::Idle);
                }
            }
        }
        Ok(())
    }
}

impl VideoDriver for PipewireDriver {
    fn video_supported(&self) -> bool {
        true
    }

    fn create_video_filter(
        &mut self,
        request: VideoFilterRequest,
    ) -> BackendResult<VideoFilterInstance> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.create_video_filter_locked(request)
        })
    }

    fn remove_video_filter(&mut self, instance_id: &str) -> BackendResult<()> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.remove_video_filter_locked(instance_id)
        })
    }

    fn set_video_filter_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        // Worker control touches no PipeWire objects; no loop lock needed.
        let bridge = self.video_bridges.get(instance_id).ok_or_else(|| {
            BackendError::Native(format!("unknown video filter instance {instance_id}"))
        })?;
        if enabled {
            if !bridge.worker_running() {
                let processor =
                    filters::create_filter(&bridge.filter_id, &bridge.params).map_err(|error| {
                        BackendError::Native(format!("video filter failed: {error}"))
                    })?;
                bridge.ensure_worker(processor);
            }
        } else {
            bridge.stop_worker();
        }
        Ok(())
    }

    fn video_filters(&self) -> Vec<VideoFilterInstance> {
        self.video_bridges
            .values()
            .map(|bridge| bridge.instance.clone())
            .collect()
    }

    fn screen_cast_node(&self) -> Option<NodeId> {
        let active = self.screen_cast.status().pipewire_node_id?;
        self.graph
            .nodes
            .keys()
            .find(|id| native_node_id(**id) == active)
            .copied()
    }

    fn video_node_info(&self, node: NodeId) -> Option<VideoNodeInfo> {
        if let Some(bridge) = self
            .video_bridges
            .values()
            .find(|bridge| bridge.node_id() == node)
        {
            return Some(bridge.info());
        }
        // The active screen-cast stream is an external node; report what the
        // preview tap negotiated so the UI can show its format.
        if let Some(preview) = &self.screen_cast_preview {
            let status = self.screen_cast.status();
            if status.pipewire_node_id == Some(native_node_id(node)) {
                let spec = preview.shared.spec.lock().ok().and_then(|spec| *spec);
                let live = preview.shared.connected.load(Ordering::Relaxed);
                return Some(VideoNodeInfo {
                    node_id: node,
                    instance_id: None,
                    spec,
                    state: if live {
                        VideoNodeState::Live
                    } else {
                        VideoNodeState::Idle
                    },
                    counters: Some(
                        preview
                            .shared
                            .diagnostics
                            .snapshot_with_queue(&preview.shared.queue),
                    ),
                    preview_state: self.screen_cast_preview_slot.state(),
                });
            }
        }
        None
    }

    fn video_preview(&self, instance_id: &str) -> Option<VideoPreview> {
        self.video_bridges
            .get(instance_id)
            .map(|bridge| bridge.preview.clone())
    }

    fn screen_cast_preview(&self) -> Option<VideoPreview> {
        self.screen_cast_preview
            .as_ref()
            .map(|_| self.screen_cast_preview_slot.clone())
    }

    fn start_screen_cast(&mut self, request: ScreenCastRequest) -> BackendResult<ScreenCastStatus> {
        // The portal dialog blocks until the user answers; never hold the
        // thread-loop lock across it.
        let status = self.screen_cast.start(&request);
        self.with_loop(|driver| {
            driver.sync()?;
            driver.reconcile_video_locked()?;
            Ok(driver.screen_cast.status())
        })?;
        Ok(status)
    }

    fn stop_screen_cast(&mut self) -> BackendResult<()> {
        self.screen_cast.stop();
        self.with_loop(|driver| {
            driver.screen_cast_preview = None;
            driver.screen_cast_preview_slot.restart();
            driver.sync()?;
            driver.reconcile_video_locked()
        })
    }

    fn screen_cast_status(&self) -> ScreenCastStatus {
        self.screen_cast.status()
    }

    fn create_virtual_display(
        &mut self,
        request: VirtualDisplayRequest,
    ) -> BackendResult<VirtualDisplayStatus> {
        crate::video::validate_virtual_display(&request)?;
        // Runtime-detect support on every attempt; compositors differ and
        // the answer can change across restarts.
        let supported = self.virtual_cast.probe_virtual_support();
        self.virtual_support = supported;
        if supported != Some(true) {
            return Ok(VirtualDisplayStatus {
                supported,
                active: false,
                error: Some("virtual displays are not supported by this compositor".into()),
            });
        }
        let status = self.virtual_cast.start(&ScreenCastRequest {
            source: ScreenCastSource::Virtual,
            show_cursor: true,
            multiple: false,
        });
        let active = status.state.is_active();
        let error = status.error.clone();
        self.with_loop(|driver| {
            driver.sync()?;
            driver.reconcile_video_locked()
        })?;
        Ok(VirtualDisplayStatus {
            supported: Some(true),
            active,
            error,
        })
    }

    fn stop_virtual_display(&mut self) -> BackendResult<()> {
        self.virtual_cast.stop();
        self.with_loop(|driver| {
            driver.sync()?;
            driver.reconcile_video_locked()
        })
    }

    fn virtual_display_status(&self) -> VirtualDisplayStatus {
        VirtualDisplayStatus {
            supported: self.virtual_support,
            active: self.virtual_cast.status().state.is_active(),
            error: self.virtual_cast.status().error.clone(),
        }
    }
}
