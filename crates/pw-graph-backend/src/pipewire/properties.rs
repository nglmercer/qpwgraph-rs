use super::*;

impl PipewireDriver {
    pub(super) fn set_node_props_locked(
        &self,
        node: NodeId,
        properties: Vec<pw::spa::pod::Property>,
    ) -> BackendResult<()> {
        let object = pw::registry::GlobalObject {
            id: native_node_id(node),
            permissions: pw::permissions::PermissionFlags::empty(),
            type_: pw::types::ObjectType::Node,
            // pipewire-rs keeps ObjectType::client_version private; version 3
            // is the public node interface version used by pipewire 0.8.
            version: NODE_INTERFACE_VERSION,
            props: None::<pw::properties::Properties>,
        };
        let proxy = self
            .registry()?
            .bind::<pw::node::Node, _>(&object)
            .map_err(|error| native_error("PipeWire node binding", error))?;
        let value = pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamProps.as_raw(),
            id: ParamType::Props.as_raw(),
            properties,
        });
        let pod_bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &value)
            .map_err(|error| native_error("PipeWire node properties serialization", error))?
            .0
            .into_inner();
        let pod = Pod::from_bytes(&pod_bytes).ok_or_else(|| {
            BackendError::Native("could not serialize PipeWire node properties".into())
        })?;
        proxy.set_param(ParamType::Props, 0, pod);
        drop(proxy);
        self.roundtrip_locked()
    }

    pub(super) fn set_node_mute_locked(&mut self, node: NodeId, muted: bool) -> BackendResult<()> {
        // Relay nodes use application-local gain/mute, not PipeWire Props.
        // The `relay` field only exists with the `relay` feature; without it
        // `is_relay_device_node` always returns false.
        #[cfg(all(target_os = "linux", feature = "relay"))]
        if let Some(node_name) = self.graph.node(node).map(|n| n.name.clone()) {
            if is_relay_device_node(&node_name) {
                if let Some(relay) = self.relay.as_ref() {
                    relay.playback_shared.set_muted(muted);
                    eprintln!("Relay playback mute: {}", muted);
                    return Ok(());
                }
            }
        }
        self.set_node_props_locked(
            node,
            vec![pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_mute,
                pw::spa::pod::Value::Bool(muted),
            )],
        )?;
        let state = self.audio_controls.entry(node).or_default();
        state.muted = Some(muted);
        state.mute_readable = true;
        Ok(())
    }

    pub(super) fn set_node_volume_locked(
        &mut self,
        node: NodeId,
        volume: f32,
    ) -> BackendResult<()> {
        // Relay nodes: linear 0.0..2.0 gain, clamped and stored in shared playback state.
        // The `relay` field only exists with the `relay` feature; without it
        // `is_relay_device_node` always returns false.
        #[cfg(all(target_os = "linux", feature = "relay"))]
        if let Some(node_name) = self.graph.node(node).map(|n| n.name.clone()) {
            if is_relay_device_node(&node_name) {
                let g = volume.clamp(0.0, 2.0);
                if let Some(relay) = self.relay.as_ref() {
                    relay.playback_shared.set_gain(g);
                    eprintln!(
                        "Relay playback gain: {}% ({:.1} dB)",
                        (g * 100.0) as u32,
                        if g > 0.0 {
                            20.0 * g.log10()
                        } else {
                            f32::NEG_INFINITY
                        }
                    );
                    return Ok(());
                }
            }
        }
        let volume = volume.clamp(0.0, PIPEWIRE_MAX_VOLUME);
        let spa_volume = ui_volume_to_spa_volume(volume);
        self.set_node_props_locked(
            node,
            vec![pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_volume,
                pw::spa::pod::Value::Float(spa_volume),
            )],
        )?;
        let state = self.audio_controls.entry(node).or_default();
        state.volume = Some(volume);
        state.volume_readable = true;
        Ok(())
    }
}
