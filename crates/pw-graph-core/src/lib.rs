//! Core graph types shared by every backend and presentation layer.

mod appearance;
mod endpoint;
mod graph;
mod id;
mod identity;
mod layout;
mod matching;
mod node;
mod relay;

#[cfg(test)]
mod tests;

pub use appearance::{NodeAppearance, WINDOWS_ENDPOINT_CAPTURE_ICON, WINDOWS_ENDPOINT_RENDER_ICON};
pub use endpoint::{
    Direction, EndpointMatchMode, EndpointResolution, EndpointSelector, NodeType, PortType,
};
pub use graph::{Graph, GraphError};
pub use id::{
    backend_for_link, backend_for_node, backend_for_port, decode_backend_local_id,
    decode_backend_namespace, encode_backend_id, BackendKind, BackendNamespace, LinkId, NodeId,
    PortId, BACKEND_SHIFT, LOCAL_ID_MASK,
};
pub use identity::NodeIdentity;
pub use node::{Link, Node, Port, PortKey};
pub use relay::{
    legacy_relay_port_name, RELAY_SINK_NODE_NAME, RELAY_SINK_PORT_ROLE, RELAY_SOURCE_NODE_NAME,
    RELAY_SOURCE_PORT_ROLE,
};
