use super::ui::UiBridge;
use super::QpwgraphApp;
use std::cell::RefCell;
use std::rc::Rc;

impl QpwgraphApp {
    pub(crate) fn refresh_graph(&mut self) {
        match self.driver.refresh() {
            Ok(nodes) => {
                self.last_graph_refresh = std::time::Instant::now();
                self.status = self.tf("status.refreshed", &[("count", nodes.len().to_string())]);
            }
            Err(error) => self.status_error("status.refresh_failed", &error),
        }
    }
}

/// Start the single Slint window and keep the backend lifecycle on Slint's
/// event thread. The bridge owns the repeating timer for backend refresh and
/// model projection.
pub(crate) fn run(args: crate::args::Args) -> Result<(), Box<dyn std::error::Error>> {
    let app = Rc::new(RefCell::new(QpwgraphApp::new(args)));
    let bridge = UiBridge::new(app)?;
    bridge.run()?;
    Ok(())
}
