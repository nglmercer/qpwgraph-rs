//! Linux video is hosted by the PipeWire child backend only.

use super::*;

impl pw_graph_backend::video::VideoDriver for CompositeDriver {
    fn video_supported(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .is_some_and(|driver| driver.video_supported())
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            false
        }
    }

    fn create_video_filter(
        &mut self,
        request: pw_graph_backend::video::VideoFilterRequest,
    ) -> BackendResult<pw_graph_backend::video::VideoFilterInstance> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            let instance = self.pipewire_mut()?.create_video_filter(request)?;
            self.rebuild_merged_graph()?;
            Ok(instance)
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = request;
            Err(Self::unsupported("video filters need the PipeWire backend"))
        }
    }

    fn remove_video_filter(&mut self, instance_id: &str) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?.remove_video_filter(instance_id)?;
            self.rebuild_merged_graph()?;
            Ok(())
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = instance_id;
            Err(Self::unsupported("video filters need the PipeWire backend"))
        }
    }

    fn set_video_filter_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?
                .set_video_filter_enabled(instance_id, enabled)
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = (instance_id, enabled);
            Err(Self::unsupported("video filters need the PipeWire backend"))
        }
    }

    fn video_filters(&self) -> Vec<pw_graph_backend::video::VideoFilterInstance> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .map(|driver| driver.video_filters())
                .unwrap_or_default()
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            Vec::new()
        }
    }

    fn video_node_info(&self, node: NodeId) -> Option<pw_graph_backend::video::VideoNodeInfo> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .and_then(|driver| driver.video_node_info(node))
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = node;
            None
        }
    }

    fn video_preview(
        &self,
        instance_id: &str,
    ) -> Option<pw_graph_backend::video::VideoPreviewHandle> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .and_then(|driver| driver.video_preview(instance_id))
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = instance_id;
            None
        }
    }

    fn screen_cast_preview(&self) -> Option<pw_graph_backend::video::VideoPreviewHandle> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .and_then(|driver| driver.screen_cast_preview())
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            None
        }
    }

    fn start_screen_cast(
        &mut self,
        request: pw_graph_backend::video::ScreenCastRequest,
    ) -> BackendResult<pw_graph_backend::video::ScreenCastStatus> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            let status = self.pipewire_mut()?.start_screen_cast(request)?;
            self.rebuild_merged_graph()?;
            Ok(status)
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = request;
            Err(Self::unsupported(
                "screen capture needs the PipeWire backend",
            ))
        }
    }

    fn stop_screen_cast(&mut self) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?.stop_screen_cast()?;
            self.rebuild_merged_graph()?;
            Ok(())
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            Err(Self::unsupported(
                "screen capture needs the PipeWire backend",
            ))
        }
    }

    fn screen_cast_status(&self) -> pw_graph_backend::video::ScreenCastStatus {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .map(|driver| driver.screen_cast_status())
                .unwrap_or_default()
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            pw_graph_backend::video::ScreenCastStatus::default()
        }
    }

    fn create_virtual_display(
        &mut self,
        request: pw_graph_backend::video::VirtualDisplayRequest,
    ) -> BackendResult<pw_graph_backend::video::VirtualDisplayStatus> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            let status = self.pipewire_mut()?.create_virtual_display(request)?;
            self.rebuild_merged_graph()?;
            Ok(status)
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = request;
            Err(Self::unsupported(
                "virtual displays need the PipeWire backend",
            ))
        }
    }

    fn stop_virtual_display(&mut self) -> BackendResult<()> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire_mut()?.stop_virtual_display()?;
            self.rebuild_merged_graph()?;
            Ok(())
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            Err(Self::unsupported(
                "virtual displays need the PipeWire backend",
            ))
        }
    }

    fn virtual_display_status(&self) -> pw_graph_backend::video::VirtualDisplayStatus {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.pipewire
                .as_ref()
                .map(|driver| driver.virtual_display_status())
                .unwrap_or_default()
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            pw_graph_backend::video::VirtualDisplayStatus::default()
        }
    }
}
