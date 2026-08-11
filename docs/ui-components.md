# Slint UI architecture

The desktop UI is compiled from [`crates/pw-graph-app/ui/main.slint`](../crates/pw-graph-app/ui/main.slint).
The build script exposes the pinned `slint-node-editor` source under the
`slint-node-editor` import name and generates the Rust bindings used by
`crates/pw-graph-app/src/app/ui.rs`.

`pw-graph-ui` deliberately has no renderer dependency. `GraphViewState` owns
filtering, selection, Easy/Advanced port grouping, meter requests, appearance,
and the dense mapping from PipeWire's `u64` IDs to Slint's `int` IDs. The Slint
bridge projects its `GraphViewSnapshot` into `VecModel<NodeData>` and
`VecModel<LinkData>` rows.

## Node editor integration

`NodeEditorSetup` wires geometry tracking, link-path computation, grid updates,
and drag completion. The application handles the four selection intents and
turns completed drags into `MoveNodesCommand`, preserving undo/redo behavior.
The upstream `.slint` files are copied into the app's UI import path at the
pinned revision so the UI source remains available to the build and can be
patched locally if the application needs a graph-specific interaction.

The bridge only updates backend-derived node rows when no drag is active. This
prevents the 50 ms backend refresh timer from replacing the mutable Slint rows
while a user is moving a node.

## Adding a control

Add visual controls and callbacks in `main.slint`, enqueue a small event in
`UiBridge::install_callbacks`, and handle it in `handle_action` or
`process_event`. Read live text and viewport properties before processing the
timer queue, then let the application state be the source for the next model
projection. Persistent settings should be copied through `sync_config` and
`AppConfig`, not stored in the Slint model.
