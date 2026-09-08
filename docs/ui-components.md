# Slint UI structure

The production desktop interface is defined in
`crates/pw-graph-slint/ui/` and is driven by the framework-neutral application
state in `crates/pw-graph-app-core/` plus the shared backend, command, config,
patchbay, effects, relay, and metering crates.

`src/bridge/` translates application models into Slint rows and translates
callbacks into application actions. It does not mutate PipeWire, ALSA, relay,
or patchbay state independently. `src/model/` contains the UI projection,
selection state, stable ID mapping, geometry inputs, layout persistence, and
node appearance projection.

`ui/node-canvas.slint` renders nodes, ports, links, minimap, and gestures.
Rust owns world coordinates and hit testing in `src/canvas.rs`, so drawing and
interaction share the same geometry. `ui/parity-components.slint` contains
dialogs, settings rows, history, rules, audio controls, localization helpers,
and the shared typography theme.

The bridge updates `UiTheme` from the shared configuration for UI, panel, and
node text scales. `UiI18n` resolves all user-visible application messages
through `pw-graph-i18n`.

Effects use typed callbacks for selection, creation, toggles, and parameter
changes. Rust owns the selected descriptor ID and projects its index only into
the ComboBox; the ComboBox's explicit `selected` callback is the only user
selection event. Effect rows and their parameter `VecModel`s are retained and
updated in place while values change, so a periodic graph sync cannot replace
an active slider component or lose pointer capture. Rapid parameter events are
coalesced by `(instance_id, parameter_id)` before they reach the backend.
