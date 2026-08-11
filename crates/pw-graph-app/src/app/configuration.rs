use super::{layout, QpwgraphApp};
use pw_graph_core::NodeAppearance;
use pw_graph_ui::MediaFilter;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

impl QpwgraphApp {
    pub(super) fn sync_config(&mut self) {
        self.config.zoom = self.canvas.zoom;
        self.config.sort_type = if self.canvas.sort_ports_by_name {
            "name".into()
        } else {
            "id".into()
        };
        self.config.sort_order = if self.canvas.sort_ports_descending {
            "descending".into()
        } else {
            "ascending".into()
        };
        self.config.thumbnail_view = self.canvas.thumbnail_mode;
        self.config.minimap_visible = self.canvas.minimap_visible;
        self.config.connect_mode = self.canvas.connect_mode.as_str().into();
        self.config.media_filter = self.canvas.media_filter.as_str().into();
        self.config.graph_search = self.canvas.search_query.clone();

        let graph = self.driver.graph();
        let mut key_counts = BTreeMap::new();
        for node in graph.nodes.values() {
            *key_counts
                .entry(layout::node_layout_key(node))
                .or_insert(0_usize) += 1;
        }
        self.config.node_positions = graph
            .nodes
            .iter()
            .map(|(id, node)| (id.0.to_string(), node.position))
            .collect();
        self.config.node_positions_by_name = graph
            .nodes
            .values()
            .filter_map(|node| {
                let key = layout::node_layout_key(node);
                (key_counts.get(&key) == Some(&1)).then_some((key, node.position))
            })
            .collect();
        self.config.node_view_by_name = graph
            .nodes
            .values()
            .filter_map(|node| {
                let key = layout::node_layout_key(node);
                if key_counts.get(&key) != Some(&1) {
                    return None;
                }
                let appearance = self.canvas.node_appearance(node.id);
                (appearance != NodeAppearance::default()).then_some((key, appearance))
            })
            .collect();

        let effect_positions: BTreeMap<_, _> = self
            .driver
            .effect_instances()
            .into_iter()
            .filter_map(|instance| {
                graph
                    .node(instance.node_id)
                    .map(|node| (instance.config.instance_id, node.position))
            })
            .collect();
        for effect in &mut self.config.effects {
            if let Some(position) = effect_positions.get(&effect.instance.instance_id) {
                effect.position = *position;
            }
        }
        self.config.patchbay_path = Some(self.patchbay_file.clone());
        self.config.patchbay_profiles.insert(
            self.config.active_patchbay_profile.clone(),
            self.patchbay_file.clone(),
        );
    }

    pub(crate) fn save_config_now(&mut self) {
        self.sync_config();
        if self.persist_report(
            self.config.save_to(&self.config_file),
            "status.config_save_failed",
        ) {
            self.config_saved_snapshot = self.config.clone();
            self.config_dirty_since = None;
            self.status = self.t("status.config_saved");
        }
    }

    pub(super) fn autosave_config(&mut self) {
        self.sync_config();
        if self.config == self.config_saved_snapshot {
            self.config_dirty_since = None;
            return;
        }
        let dirty_since = self.config_dirty_since.get_or_insert_with(Instant::now);
        if dirty_since.elapsed() < Duration::from_millis(500) {
            return;
        }
        if self.persist_report(
            self.config.save_to(&self.config_file),
            "status.config_save_failed",
        ) {
            self.config_saved_snapshot = self.config.clone();
            self.config_dirty_since = None;
        } else {
            self.config_dirty_since = Some(Instant::now());
        }
    }

    pub(super) fn update_canvas_from_config(&mut self) {
        self.canvas.media_filter = MediaFilter::parse(&self.config.media_filter);
        self.canvas.sort_ports_by_name = self.config.sort_type != "id";
        self.canvas.sort_ports_descending = self.config.sort_order == "descending";
        self.canvas.node_text_scale = self.config.node_text_scale;
        self.canvas.repel_overlapping_nodes = self.config.repel_overlapping_nodes;
        self.canvas.connect_through_nodes = self.config.connect_through_nodes;

        let graph = self.driver.graph();
        let mut key_counts = BTreeMap::new();
        for node in graph.nodes.values() {
            *key_counts
                .entry(layout::node_layout_key(node))
                .or_insert(0_usize) += 1;
        }
        for node in graph.nodes.values() {
            let key = layout::node_layout_key(node);
            let appearance = if key_counts.get(&key) == Some(&1) {
                self.config
                    .node_view_by_name
                    .get(&key)
                    .map(|view| NodeAppearance {
                        collapsed: view.collapsed,
                        custom_name: view.custom_name.clone(),
                        color: view.color,
                    })
                    .unwrap_or_default()
            } else {
                NodeAppearance::default()
            };
            self.canvas.set_node_appearance(node.id, appearance);
        }
        self.config.thumbnail_view = self.canvas.thumbnail_mode;
        self.config.minimap_visible = self.canvas.minimap_visible;
        self.config.connect_mode = self.canvas.connect_mode.as_str().into();
    }

    pub(super) fn update_window_size(&mut self, width: f32, height: f32) {
        if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
            self.config.window_width = width;
            self.config.window_height = height;
        }
    }
}
