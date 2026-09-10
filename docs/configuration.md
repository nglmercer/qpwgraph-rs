# Configuration and patchbay files

Where qpwgraph-rs keeps its state, what it restores at startup, and what it
deliberately does not.

## Application configuration

The application reads the existing qpwgraph-rs TOML configuration and writes
it back without discarding unknown fields. Node positions and appearance use
stable numeric/name keys. Volume and mute are live controls and are not
silently restored at startup. Configuration is stored under
`~/.config/qpwgraph-rs` on Linux and `%APPDATA%\qpwgraph-rs` on Windows.

Preserving unknown fields is what lets an older and a newer build share a
configuration file without either one stripping the other's settings.

Pairing PINs are the exception to persistence: the host and client relay PINs
are held in memory only and never written to disk. The relay installation ID
is persisted so a peer remains recognizable across Wi-Fi/USB address changes.
After explicit PIN pairing, per-peer trusted credentials are persisted as
owner-only hex values through the same atomic config writer. They are used only
when discovery presents the same stable peer ID; arbitrary discovered peers are
never auto-connected. Set `relay_auto_connect_trusted = false` to keep trusted
peers manual. The desktop relay panel also offers a Forget action, which
revokes the live credential before removing the config record. The stable
installation ID is independent of this list, so forgetting a peer does not
regenerate identity; only an explicit reset/reinstall does.

Android stores the equivalent installation ID separately from encrypted trusted
credentials. The credentials are protected by Android Keystore AES-GCM, while
the `relay` preferences file (`sharedpref/relay.xml`) is excluded from cloud
backup and device-to-device transfer. Existing plaintext records are migrated
only after the encrypted replacement commits successfully; a failed migration
leaves the old record recoverable for retry. Android's global trusted
auto-connect switch defaults on for trusted USB candidates, while trusted
Wi-Fi reconnect is separately opt-in.

## Patchbay files

Patchbay files retain the qpwgraph XML shape for `.qpwgraph` and `.xml` files;
other extensions use JSON. Save/load use native dialogs. The active path,
recent files, named profiles, editable rules, auto-pin, exclusive activation,
auto-disconnect, and startup activation are persisted. Live graph changes,
undo, and redo keep the saved patchbay state synchronized.

### Dynamic application routes

Patchbay JSON is now schema version 2. Existing display fields remain, and new
rules additionally retain typed endpoint selectors. A selector prefers, in
order, an effect instance ID, `application.id`, process binary plus application
name, stream role/channel, and then a stable node name. `object.serial` is only
a current-session refinement. PipeWire global IDs, `client.id`, PIDs, and
object serials are never treated as cross-restart durable identities.

PipeWire application properties may live on the parent Client object. The
registry joins Client metadata to its nodes, with node-level properties taking
precedence. In practice Discord/WebRTC may omit `application.id`, so the
process/application fallback is intentional. `media.name` is only a display or
low-confidence hint because browsers and players can change it per track.

An activated patchbay is a desired-state policy: it continuously maintains
saved routes after debounced graph changes (about 100 ms). A disappearing
application leaves its rule persisted as **Waiting for endpoint**; when its
replacement node and ports appear, the rule is resolved and the missing link
is recreated. Equal best candidates are **Ambiguous** and fail closed rather
than choosing the largest PipeWire ID. Transient backend errors retry with a
bounded backoff. Application selectors intentionally surface duplicate
compatible streams as ambiguous; they do not silently fan out. The optional
name-pattern mode is scoped to an explicit selector and currently supports a
safe prefix match rather than globally merging same-named nodes.

Manual disconnect has desired-state semantics: deleting a saved route removes
the desired rule (and suppresses the just-removed live pair), so reconciliation
does not recreate it 100 ms later. Explicit reconnect clears that suppression.
Exclusive cleanup waits until all desired endpoints are resolved and the graph
has settled; immutable or merely observed links are not torn down.

Standard qpwgraph XML continues to contain only its interoperable `output` and
`input` node/port fields. Rich qpwgraph-rs selectors for XML are kept in a
deterministic `.qpwgraph-rs-selectors.json` sidecar next to the XML file. A
missing sidecar is harmless, and old XML/JSON files load through the legacy
name-based selector path. When a legacy rule uniquely resolves to richer
identity, the next qpwgraph-rs save can preserve that enrichment.

Node positions use the same v2 identity idea: effect instance, application ID
plus role, process/application plus role, and only then the legacy
`NodeType:node.name` key. A unique legacy key is used for migration; genuinely
indistinguishable recreated streams are placed automatically instead of
randomly exchanging saved positions.

The Patchbay **Debug** action opens a copyable report showing selectors,
current endpoint properties, resolver confidence, and reconciliation states.

## Related

- [Effects and metering](effects-and-metering.md) — startup restoration order.
- [Audio relay](audio-relay.md) — relay endpoint selection and its persistence.
