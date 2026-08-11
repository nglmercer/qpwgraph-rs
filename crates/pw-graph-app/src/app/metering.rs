use super::QpwgraphApp;
use pw_graph_backend::MeterPolicy;
use pw_graph_ui::MeterReading;
use std::collections::BTreeSet;

impl QpwgraphApp {
    pub(super) fn sync_meter_policy(&mut self) {
        let policy = MeterPolicy::parse(&self.config.audio_meters);
        if policy == self.meter_policy {
            return;
        }
        self.meter_policy = policy;
        self.canvas.metering_disabled = policy == MeterPolicy::Disabled;
        if let Err(error) = self.driver.set_meter_policy(policy) {
            self.status = self.tf(
                "status.meter_policy_failed",
                &[("error", error.to_string())],
            );
        }
    }

    pub(super) fn request_visible_meters(&mut self, window_visible: bool) {
        if self.meter_policy != MeterPolicy::OnDemand {
            return;
        }
        let requested = if window_visible {
            self.canvas.requested_meter_nodes(self.driver.graph())
        } else {
            BTreeSet::new()
        };
        let _ = self.driver.request_meters(&requested);
    }

    pub(crate) fn reset_audio_config(&mut self) {
        self.canvas.pinned_meter = None;
        self.canvas.meters.clear();
        self.canvas.port_meters.clear();
        match self.driver.reset_audio_config() {
            Ok(()) => self.status = self.t("status.audio_reset"),
            Err(error) => self.status_error("status.audio_reset_failed", &error),
        }
    }

    pub(super) fn refresh_audio_meters(&mut self) {
        let readings = match self.driver.audio_meters() {
            Ok(readings) => readings,
            Err(_) => {
                self.canvas.meters.clear();
                return;
            }
        };
        self.canvas.meters.clear();
        self.canvas.port_meters.clear();
        for meter in readings {
            let reading = MeterReading {
                rms: meter.rms,
                peak: meter.peak,
                age_ms: meter.age_ms,
                available: meter.available,
            };
            self.canvas.meters.insert(meter.node_id, reading);
            if let Some(port_id) = meter.port_id {
                self.canvas.port_meters.insert(port_id, reading);
            }
        }
        if self
            .canvas
            .pinned_meter
            .is_some_and(|port| self.driver.graph().port(port).is_none())
        {
            self.canvas.pinned_meter = None;
        }
    }
}
