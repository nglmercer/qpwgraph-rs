//! PipeWire effect lifecycle bridge.
//!
//! Preparation is owned by the shared bounded component manager. This module
//! only translates backend calls into lifecycle events and performs the small
//! control-plane operations that are safe after preparation.

use super::PipewireDriver;
use crate::{
    BackendError, BackendResult, EffectCreateRequest, EffectDriver, EffectEvent,
    EffectInsertRequest, EffectInstance, EffectNodeRequest, EffectTicket,
};
use pw_graph_effects::EffectDescriptor;

impl EffectDriver for PipewireDriver {
    fn effect_descriptors(&self) -> Vec<EffectDescriptor> {
        self.effect_host.descriptors()
    }

    fn effect_instances(&self) -> Vec<EffectInstance> {
        self.effects
            .values()
            .map(super::effects::NativeEffect::snapshot)
            .collect()
    }

    fn effect_diagnostics(&self, instance_id: &str) -> BackendResult<Option<String>> {
        let effect = self
            .effects
            .get(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        Ok(effect.full_diagnostics())
    }

    fn supports_effect_nodes(&self) -> bool {
        true
    }

    fn begin_create_effect(&mut self, request: EffectCreateRequest) -> BackendResult<EffectTicket> {
        self.begin_create_effect_async(request)
    }

    fn poll_effect_events(&mut self) -> BackendResult<Vec<EffectEvent>> {
        self.poll_effect_lifecycle()
    }

    fn cancel_effect(&mut self, ticket: EffectTicket) -> BackendResult<()> {
        if self.effect_loader.cancel(ticket) {
            Ok(())
        } else {
            Err(BackendError::native(format!(
                "unknown or completed effect ticket {}",
                ticket.0
            )))
        }
    }

    fn create_effect_node(&mut self, request: EffectNodeRequest) -> BackendResult<EffectInstance> {
        let _ = request;
        Err(BackendError::unsupported(
            "PipeWire effects must be created asynchronously with begin_create_effect",
        ))
    }

    fn insert_effect(&mut self, request: EffectInsertRequest) -> BackendResult<EffectInstance> {
        let _ = request;
        Err(BackendError::unsupported(
            "PipeWire effects must be inserted asynchronously with begin_create_effect",
        ))
    }

    fn set_effect_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        let effect = self
            .effects
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        effect.set_enabled(enabled);
        Ok(())
    }

    fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        let effect = self
            .effects
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        effect.set_parameter(parameter, value)
    }

    fn remove_effect(&mut self, instance_id: &str) -> BackendResult<()> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.remove_effect_locked(instance_id)
        })
    }
}
