//! Reading node volume and mute back from PipeWire.
//!
//! Writing a control is a fire-and-forget `set_param`, but reading one means
//! binding the node proxy, asking for its `Props`, and waiting for the reply.
//! Until this existed the driver only knew the values it had written itself,
//! so a level set anywhere else -- pavucontrol, a media key, another app --
//! was reported as unknown.
//!
//! The read happens once per graph rebuild rather than per query: every node
//! is bound, every request is queued, and a single roundtrip collects all the
//! replies. Rebuilds are event-driven, so this is not a poll.

use super::*;
use crate::api::spa_volume_to_ui_volume;
use pw::spa::pod::deserialize::PodDeserializer;

/// One node's controls as PipeWire reported them.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PropsReading {
    pub(super) volume: Option<f32>,
    pub(super) muted: Option<bool>,
}

impl PropsReading {
    fn is_empty(&self) -> bool {
        self.volume.is_none() && self.muted.is_none()
    }
}

/// Merge only the properties the daemon actually returned. A Props object is
/// allowed to omit either field, and omission is not evidence for a default
/// volume or mute value.
fn apply_props_reading(state: &mut NodeAudioState, reading: PropsReading) {
    if let Some(volume) = reading.volume {
        state.volume = Some(volume.clamp(0.0, PIPEWIRE_MAX_VOLUME));
        state.volume_readable = true;
    }
    if let Some(muted) = reading.muted {
        state.muted = Some(muted);
        state.mute_readable = true;
    }
}

/// Pull `volume`, `channelVolumes` and `mute` out of a `Props` object pod.
///
/// `channelVolumes` wins when present: a device with per-channel gain leaves
/// the single `volume` property at unity, so trusting it would report full
/// volume for a node the user has turned down.
pub(super) fn parse_props(bytes: &[u8]) -> PropsReading {
    let mut reading = PropsReading::default();
    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(bytes) else {
        return reading;
    };
    let mut channel_volume = None;
    for property in object.properties {
        match property.key {
            pw::spa::sys::SPA_PROP_volume => {
                if let Value::Float(value) = property.value {
                    reading.volume = Some(spa_volume_to_ui_volume(value));
                }
            }
            pw::spa::sys::SPA_PROP_mute => {
                if let Value::Bool(value) = property.value {
                    reading.muted = Some(value);
                }
            }
            pw::spa::sys::SPA_PROP_channelVolumes => {
                if let Value::ValueArray(pw::spa::pod::ValueArray::Float(values)) = property.value {
                    // Channels are normally uniform; the loudest is the one a
                    // single fader should show.
                    channel_volume = values
                        .iter()
                        .copied()
                        .fold(None::<f32>, |best, value| {
                            Some(best.map_or(value, |best| best.max(value)))
                        })
                        .map(spa_volume_to_ui_volume);
                }
            }
            _ => {}
        }
    }
    if let Some(value) = channel_volume {
        reading.volume = Some(value);
    }
    reading
}

impl PipewireDriver {
    /// Refresh the cached controls for every node in the current graph.
    ///
    /// Called from the graph rebuild, which already holds the loop lock. A
    /// node that does not answer keeps whatever was known before, so a
    /// transient failure does not blank a card that was reading fine.
    pub(super) fn read_node_controls_locked(&mut self) {
        let Ok(registry) = self.registry() else {
            return;
        };
        // Effect nodes deliberately do not expose writable PipeWire audio
        // Props; their state is owned by the callback runtime. Avoid binding
        // a temporary Node proxy for them. In addition to unnecessary work,
        // an outstanding Props request can race a filter being destroyed and
        // make PipeWire report `unknown resource` during effect removal.
        let nodes: Vec<NodeId> = self
            .graph
            .nodes
            .values()
            .filter(|node| node.node_type != NodeType::Effect)
            .map(|node| node.id)
            .collect();
        if nodes.is_empty() {
            return;
        }

        // Proxies and listeners have to outlive the roundtrip, so they are
        // held here until every reply has landed.
        let mut proxies = Vec::with_capacity(nodes.len());
        let mut listeners = Vec::with_capacity(nodes.len());
        let readings: Rc<RefCell<BTreeMap<NodeId, PropsReading>>> =
            Rc::new(RefCell::new(BTreeMap::new()));

        for node_id in nodes {
            let native_id = native_node_id(node_id);
            let object = pw::registry::GlobalObject {
                id: native_id,
                permissions: pw::permissions::PermissionFlags::empty(),
                type_: pw::types::ObjectType::Node,
                version: NODE_INTERFACE_VERSION,
                props: None::<pw::properties::Properties>,
            };
            let Ok(proxy) = registry.bind::<pw::node::Node, _>(&object) else {
                continue;
            };
            let sink = readings.clone();
            let state = self.state.clone();
            let listener = proxy
                .add_listener_local()
                .info(move |info| {
                    let Some(props) = info.props() else {
                        return;
                    };
                    let Ok(mut state) = state.lock() else {
                        return;
                    };
                    if let Some(record) = state.nodes.get_mut(&native_id) {
                        record.update_from_node_info(props);
                    }
                })
                .param(move |_seq, id, _index, _next, param| {
                    if id != ParamType::Props {
                        return;
                    }
                    let Some(param) = param else {
                        return;
                    };
                    let reading = parse_props(param.as_bytes());
                    if !reading.is_empty() {
                        sink.borrow_mut().insert(node_id, reading);
                    }
                })
                .register();
            proxy.enum_params(0, Some(ParamType::Props), 0, u32::MAX);
            proxies.push(proxy);
            listeners.push(listener);
        }

        // One roundtrip for the whole graph rather than one per node.
        let _ = self.roundtrip_locked();
        drop(listeners);
        drop(proxies);

        for (node_id, reading) in readings.borrow().iter() {
            let state = self.audio_controls.entry(*node_id).or_default();
            apply_props_reading(state, *reading);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_only_props_keep_mute_unknown() {
        let mut state = NodeAudioState::UNSUPPORTED;
        apply_props_reading(
            &mut state,
            PropsReading {
                volume: Some(0.5),
                muted: None,
            },
        );
        assert_eq!(state.volume, Some(0.5));
        assert!(state.volume_readable);
        assert_eq!(state.muted, None);
        assert!(!state.mute_readable);
    }

    #[test]
    fn mute_only_props_keep_volume_unknown() {
        let mut state = NodeAudioState::UNSUPPORTED;
        apply_props_reading(
            &mut state,
            PropsReading {
                volume: None,
                muted: Some(true),
            },
        );
        assert_eq!(state.muted, Some(true));
        assert!(state.mute_readable);
        assert_eq!(state.volume, None);
        assert!(!state.volume_readable);
    }

    #[test]
    fn both_props_are_applied_independently() {
        let mut state = NodeAudioState::UNSUPPORTED;
        apply_props_reading(
            &mut state,
            PropsReading {
                volume: Some(0.75),
                muted: Some(false),
            },
        );
        assert_eq!(state.volume, Some(0.75));
        assert_eq!(state.muted, Some(false));
        assert!(state.volume_readable && state.mute_readable);
    }

    #[test]
    fn missing_props_do_not_change_existing_state() {
        let mut state = NodeAudioState::readable(0.8, true);
        apply_props_reading(&mut state, PropsReading::default());
        assert_eq!(state, NodeAudioState::readable(0.8, true));
    }

    #[test]
    fn a_pod_that_is_not_a_props_object_reads_as_nothing() {
        assert!(parse_props(&[]).is_empty());
        assert!(parse_props(&[0xff, 0x00, 0x11]).is_empty());
    }
}
