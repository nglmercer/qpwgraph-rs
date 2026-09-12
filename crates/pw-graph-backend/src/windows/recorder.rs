//! Windows-specific graph projection for the platform-neutral recorder.
//!
//! WASAPI sources and sinks remain owned by `WindowsRouting`; this module only
//! gives a recorder a stable graph identity and stores the writer associated
//! with its destination port.

use super::*;
use crate::router::RecorderWriter;

#[derive(Debug)]
pub(super) struct NativeRecorder {
    pub(super) request: RecorderCreateRequest,
    pub(super) instance: RecorderInstance,
    pub(super) writer: RecorderWriter,
}

pub(super) fn draw_recorder(
    graph: &mut Graph,
    instance: &RecorderInstance,
    name: &str,
    position: [f32; 2],
) -> BackendResult<()> {
    let mut node = Node::new(instance.node_id, name.to_owned(), NodeType::Recorder)
        .with_serial(stable_local_id(&format!("recorder:{}", instance.id)));
    node.position = position;
    graph.add_node(node)?;
    graph.add_port(Port::new(
        instance.input_port,
        instance.node_id,
        "record",
        Direction::Sink,
        PortType::Audio,
    ))?;
    Ok(())
}
