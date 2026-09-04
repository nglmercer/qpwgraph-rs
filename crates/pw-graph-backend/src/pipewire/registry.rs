use super::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Mirror the daemon's registry into [`RegistryState`].
///
/// PipeWire announces every node, port and link as a "global" and withdraws
/// it by id. The callbacks run on the thread loop, so they only record what
/// changed and set the dirty flag; the driver rebuilds its graph from the
/// state on its next pass rather than doing that work on the loop thread.
///
/// The returned listener owns the subscription: dropping it unsubscribes,
/// which is why the driver keeps it alive in a field.
pub(super) fn install_registry_listener(
    registry: &pw::registry::Registry,
    state: &Arc<Mutex<RegistryState>>,
    registry_dirty: &Arc<AtomicBool>,
) -> pw::registry::Listener {
    let state_for_globals = state.clone();
    let state_for_removals = state.clone();
    let dirty_for_globals = registry_dirty.clone();
    let dirty_for_removals = registry_dirty.clone();
    registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else {
                return;
            };
            let mut state = state_for_globals.lock().unwrap();
            match &global.type_ {
                pw::types::ObjectType::Node => {
                    let name = props
                        .get(NODE_NAME)
                        .or_else(|| props.get(NODE_DESCRIPTION))
                        .unwrap_or("PipeWire node")
                        .to_owned();
                    let media_class = props.get(MEDIA_CLASS).unwrap_or_default().to_owned();
                    let serial = props
                        .get(OBJECT_SERIAL)
                        .and_then(|value| value.parse().ok());
                    state.nodes.insert(
                        global.id,
                        NodeRecord {
                            name,
                            media_class,
                            serial,
                        },
                    );
                }
                pw::types::ObjectType::Port => {
                    let media_type = props
                        .get(MEDIA_TYPE)
                        .or_else(|| props.get(FORMAT_DSP))
                        .unwrap_or_default()
                        .to_owned();
                    let direction = if props.get(PORT_DIRECTION) == Some("out") {
                        Direction::Source
                    } else {
                        Direction::Sink
                    };
                    let node_id = props
                        .get(NODE_ID)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    state.ports.insert(
                        global.id,
                        PortRecord {
                            node_id,
                            name: props.get(PORT_NAME).unwrap_or("PipeWire port").to_owned(),
                            channel: props.get(AUDIO_CHANNEL).map(str::to_owned),
                            direction,
                            media_type,
                        },
                    );
                }
                pw::types::ObjectType::Link => {
                    let output_port = props
                        .get(LINK_OUTPUT_PORT)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    let input_port = props
                        .get(LINK_INPUT_PORT)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    state.links.insert(
                        global.id,
                        LinkRecord {
                            output_port,
                            input_port,
                        },
                    );
                }
                _ => {}
            }
            dirty_for_globals.store(true, Ordering::Relaxed);
        })
        .global_remove(move |id| {
            let mut state = state_for_removals.lock().unwrap();
            state.nodes.remove(&id);
            state.ports.remove(&id);
            state.links.remove(&id);
            dirty_for_removals.store(true, Ordering::Relaxed);
        })
        .register()
}

/// A PipeWire metadata object and its listener must live together. The
/// listener callback is invoked on the same thread loop as the registry, so an
/// Rc-backed collection is sufficient and avoids putting raw PipeWire
/// proxies behind a cross-thread mutex.
pub(super) struct MetadataBinding {
    /// Drop the listener before its metadata proxy.
    pub(super) listener: pw::metadata::MetadataListener,
    pub(super) metadata: pw::metadata::Metadata,
}

pub(super) type MetadataBindings = Rc<RefCell<Vec<(u32, MetadataBinding)>>>;

/// Subscribe to WirePlumber's `default` metadata object. The registry
/// listener is deliberately separate from the graph listener because binding
/// a metadata proxy is a PipeWire operation and the graph callback must stay
/// a small state mirror.
pub(super) fn install_default_metadata_listener(
    registry: &pw::registry::Registry,
    state: &Arc<Mutex<RegistryState>>,
    registry_dirty: &Arc<AtomicBool>,
) -> (pw::registry::Listener, MetadataBindings) {
    let bindings = Rc::new(RefCell::new(Vec::new()));
    // Registry callbacks require a `'static` closure, while the registry is
    // owned by PipewireDriver and outlives this listener. Store its address as
    // an integer so the callback does not acquire a misleading Send/Sync
    // bound; it is only dereferenced on the PipeWire loop thread.
    let registry_address = registry as *const pw::registry::Registry as usize;
    let bindings_for_global = bindings.clone();
    let state_for_global = state.clone();
    let dirty_for_global = registry_dirty.clone();
    let bindings_for_remove = bindings.clone();
    let state_for_remove = state.clone();
    let dirty_for_remove = registry_dirty.clone();
    let listener = registry
        .add_listener_local()
        .global(move |global| {
            if !matches!(global.type_, pw::types::ObjectType::Metadata) {
                return;
            }
            let is_default = global
                .props
                .and_then(|props| props.get("metadata.name"))
                .map_or(true, |name| name == "default");
            if !is_default {
                return;
            }
            let metadata = unsafe {
                (&*(registry_address as *const pw::registry::Registry))
                    .bind::<pw::metadata::Metadata, _>(global)
            };
            let Ok(metadata) = metadata else {
                return;
            };
            let state_for_property = state_for_global.clone();
            let dirty_for_property = dirty_for_global.clone();
            let metadata_listener = metadata
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    let Some(key) = key else {
                        return 0;
                    };
                    let Ok(mut state) = state_for_property.lock() else {
                        return 0;
                    };
                    match key {
                        "default.audio.source" => {
                            state.default_source = parse_default_device(value);
                        }
                        "default.audio.sink" => {
                            state.default_sink = parse_default_device(value);
                        }
                        _ => return 0,
                    }
                    dirty_for_property.store(true, Ordering::Relaxed);
                    0
                })
                .register();
            bindings_for_global.borrow_mut().push((
                global.id,
                MetadataBinding {
                    listener: metadata_listener,
                    metadata,
                },
            ));
        })
        .global_remove(move |id| {
            let removed = {
                let mut bindings = bindings_for_remove.borrow_mut();
                let before = bindings.len();
                bindings.retain(|(binding_id, _)| *binding_id != id);
                bindings.len() != before
            };
            if removed {
                if let Ok(mut state) = state_for_remove.lock() {
                    state.default_source = None;
                    state.default_sink = None;
                    dirty_for_remove.store(true, Ordering::Relaxed);
                }
            }
        })
        .register();
    (listener, bindings)
}

#[derive(Clone, Debug, Default)]
pub(super) struct NodeRecord {
    pub(super) name: String,
    pub(super) media_class: String,
    /// `object.serial` is unique for the lifetime of the daemon, while node
    /// names are not. Targeting by serial keeps a meter pinned to the node the
    /// user actually asked about when several share a name.
    pub(super) serial: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PortRecord {
    pub(super) node_id: u32,
    pub(super) name: String,
    pub(super) channel: Option<String>,
    pub(super) direction: Direction,
    pub(super) media_type: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct LinkRecord {
    pub(super) output_port: u32,
    pub(super) input_port: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DefaultDevice {
    pub(super) name: String,
    pub(super) serial: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RegistryState {
    pub(super) nodes: BTreeMap<u32, NodeRecord>,
    pub(super) ports: BTreeMap<u32, PortRecord>,
    pub(super) links: BTreeMap<u32, LinkRecord>,
    pub(super) default_source: Option<DefaultDevice>,
    pub(super) default_sink: Option<DefaultDevice>,
}

/// Parse WirePlumber's `Spa:String:JSON` default-device value. A few older
/// metadata providers send the node name as a plain string, so that form is
/// accepted too; an absent value means the default was cleared.
pub(super) fn parse_default_device(value: Option<&str>) -> Option<DefaultDevice> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(object) = serde_json::from_str::<serde_json::Value>(value) {
        if let Some(object) = object.as_object() {
            let name = object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| object.get("node.name").and_then(serde_json::Value::as_str))?;
            let serial = object.get("serial").and_then(|serial| {
                serial
                    .as_u64()
                    .or_else(|| serial.as_str().and_then(|value| value.parse().ok()))
            });
            return Some(DefaultDevice {
                name: name.to_owned(),
                serial,
            });
        }
        if let Some(name) = object.as_str() {
            return Some(DefaultDevice {
                name: name.to_owned(),
                serial: None,
            });
        }
    }
    Some(DefaultDevice {
        name: value.trim_matches('"').to_owned(),
        serial: None,
    })
}

pub(super) fn classify_port_type(media_type: &str, node_media_class: Option<&str>) -> PortType {
    let media_type = media_type.to_ascii_lowercase();
    let node_media_class = node_media_class.unwrap_or_default().to_ascii_lowercase();
    if media_type.contains("midi") {
        PortType::MidiJack
    } else if media_type.contains("video") {
        PortType::Video
    } else if media_type.contains("audio") {
        PortType::Audio
    } else if node_media_class.contains("midi") {
        PortType::MidiJack
    } else if node_media_class.contains("video") {
        PortType::Video
    } else if node_media_class.contains("audio") {
        PortType::Audio
    } else {
        PortType::Unknown
    }
}
