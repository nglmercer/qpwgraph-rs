//! Effects are hosted by whichever child backend supports them.

use super::*;

impl pw_graph_backend::EffectDriver for CompositeDriver {
    fn effect_descriptors(&self) -> Vec<pw_graph_effects::EffectDescriptor> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .map(|driver| driver.effect_descriptors())
                .unwrap_or_default()
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_ref()
                .map(|driver| driver.effect_descriptors())
                .unwrap_or_default()
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            Vec::new()
        }
    }

    fn effect_instances(&self) -> Vec<pw_graph_backend::EffectInstance> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .map(|driver| driver.effect_instances())
                .unwrap_or_default()
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_ref()
                .map(|driver| driver.effect_instances())
                .unwrap_or_default()
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            Vec::new()
        }
    }

    fn supports_effect_nodes(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .is_some_and(|driver| driver.supports_effect_nodes())
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_ref()
                .is_some_and(|driver| driver.supports_effect_nodes())
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            false
        }
    }

    fn begin_create_effect(
        &mut self,
        request: pw_graph_backend::EffectCreateRequest,
    ) -> BackendResult<pw_graph_backend::EffectTicket> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?.begin_create_effect(request)
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .begin_create_effect(request)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = request;
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }

    fn poll_effect_events(&mut self) -> BackendResult<Vec<pw_graph_backend::EffectEvent>> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?.poll_effect_events()
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .map(|driver| driver.poll_effect_events())
                .unwrap_or_else(|| Ok(Vec::new()))
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            Ok(Vec::new())
        }
    }

    fn cancel_effect(&mut self, ticket: pw_graph_backend::EffectTicket) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?.cancel_effect(ticket)
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .cancel_effect(ticket)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = ticket;
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }

    fn create_effect_node(
        &mut self,
        request: pw_graph_backend::EffectNodeRequest,
    ) -> BackendResult<pw_graph_backend::EffectInstance> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.mutate_pipewire(|driver| driver.create_effect_node(request))
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .create_effect_node(request)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = request;
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }

    fn insert_effect(
        &mut self,
        request: pw_graph_backend::EffectInsertRequest,
    ) -> BackendResult<pw_graph_backend::EffectInstance> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.mutate_pipewire(|driver| driver.insert_effect(request))
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .insert_effect(request)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = request;
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }

    fn set_effect_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?
                .set_effect_enabled(instance_id, enabled)
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .set_effect_enabled(instance_id, enabled)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = (instance_id, enabled);
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }

    fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?
                .set_effect_parameter(instance_id, parameter, value)
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .set_effect_parameter(instance_id, parameter, value)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = (instance_id, parameter, value);
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }

    fn remove_effect(&mut self, instance_id: &str) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.mutate_pipewire(|driver| driver.remove_effect(instance_id))
        }
        #[cfg(target_os = "windows")]
        {
            self.windows_audio
                .as_mut()
                .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
                .remove_effect(instance_id)
        }
        #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
        {
            let _ = instance_id;
            Err(Self::unsupported("effect processing is unavailable"))
        }
    }
}
