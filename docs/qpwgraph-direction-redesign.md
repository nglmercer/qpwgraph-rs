# QPWGraph Relay — Direction-First Redesign

Baseline inspected: `nglmercer/qpwgraph-rs` `main` at commit `83ac4d114f75e7716c522dc9db48695cfa625d33`.

## Goal

Replace user-selected relay roles with a single audio direction. Roles become internal implementation details.

| Direction | Mobile | Desktop | Audio |
|---|---|---|---|
| Mobile → PC | Emitter | Host | one-way mobile to desktop |
| PC → Mobile | Host | Emitter | one-way desktop to mobile |

`both` must never be a selectable or active audio role.

## State model

Introduce a shared direction enum at every frontend boundary:

```text
AudioDirection
  MobileToDesktop
  DesktopToMobile
```

Derived effective roles:

```text
Android:
  MobileToDesktop  => client + emit_only
  DesktopToMobile  => host + receive_only

Desktop:
  MobileToDesktop  => host + receive_only
  DesktopToMobile  => client + emit_only
```

Persist `audio_direction`, never `role`.

Migration:
- Android persisted `role=emit` => `mobile_to_desktop`
- Android persisted `role=receive` => `desktop_to_mobile`
- Android persisted `role=both` => `mobile_to_desktop`
- Desktop persisted `relay_role=emit` => `pc_to_mobile`
- Desktop persisted `relay_role=receive` => `mobile_to_pc`
- Desktop persisted `relay_role=both` => `mobile_to_pc`

Keep reading the legacy key for one release, but stop writing it.

## Android changes

### RelayModel.kt

Remove `RelaySettings.role` as user-owned state.

Add:

```kotlin
enum class AudioDirection {
    MobileToDesktop,
    DesktopToMobile,
}

enum class EffectiveAudioRole {
    Emitter,
    Host,
}

fun AudioDirection.androidRole(): EffectiveAudioRole =
    when (this) {
        AudioDirection.MobileToDesktop -> EffectiveAudioRole.Emitter
        AudioDirection.DesktopToMobile -> EffectiveAudioRole.Host
    }

fun AudioDirection.androidClientRole(): String =
    when (this) {
        AudioDirection.MobileToDesktop -> "emit"
        AudioDirection.DesktopToMobile -> "receive"
    }
```

Add `direction` and `switchingDirection` to `RelayUiState`.

`RelayMode` should no longer be the role authority. Discovery can stay a secondary screen or sheet.

### RelaySettingsRepository.kt

Persist:

```text
audio_direction = mobile_to_desktop | desktop_to_mobile
```

Do not persist `role`.

### RelayViewModel.kt

The existing `operationMutex` is the right serialization point. Direction changes go through one transaction.

Pseudo-flow:

```kotlin
fun setDirection(next: AudioDirection) {
    if (next == state.value.direction || state.value.switchingDirection) return

    viewModelScope.launch(Dispatchers.IO) {
        val resume = operationMutex.withLock {
            val old = mutableState.value.direction
            val wasClientLive = client.isOpen ||
                mutableState.value.connection == RelayConnectionState.Connected ||
                mutableState.value.connection == RelayConnectionState.Connecting
            val wasHostLive = host.isOpen ||
                mutableState.value.hostState == RelayHostState.Running ||
                mutableState.value.hostState == RelayHostState.Starting

            setState { it.copy(direction = next, switchingDirection = true) }
            settings.saveDirection(next)

            when (old) {
                AudioDirection.MobileToDesktop -> stopClientLocked()
                AudioDirection.DesktopToMobile -> stopHostLocked()
            }

            ResumePlan(
                startNewSide = wasClientLive || wasHostLive,
                direction = next,
            )
        }

        // Start outside the first locked section because connect/startHost
        // already acquire operationMutex.
        if (resume.startNewSide) {
            when (resume.direction) {
                AudioDirection.MobileToDesktop -> reconnectConfiguredOrTrusted()
                AudioDirection.DesktopToMobile -> startHost()
            }
        }

        setState { it.copy(switchingDirection = false) }
    }
}
```

Important:
- mobile client service role is always `"emit"` in Mobile → PC.
- mobile host service role is always `"receive"` in PC → Mobile.
- replace the current host service `role = "both"` with `"receive"`.
- reject any incoming configuration that requests `both`.

### Mobile UI

Replace Receiver / Emitter / Discover role tabs with:

1. Header/status
   - peer name
   - link (USB/Wi-Fi/LAN)
   - connection state
   - non-editable chip: `Sending` or `Receiving`

2. Large direction segmented control
   - `Phone → PC`
   - `PC → Phone`

3. Primary connection card
   - active endpoint
   - connect/disconnect
   - trusted peer / QR pairing
   - no role dropdown

4. Live audio card
   - level meter
   - codec
   - transport
   - channel state

5. Source card, only when Phone → PC
   - Microphone
   - Device playback

6. Advanced/settings
   - codec
   - frame size
   - transport
   - trusted auto-connect
   - never role

Discovery/trusted devices should be a section or secondary page, not a third role tab.

## Desktop changes

### Slint relay panel

Replace:

```text
Connections | Host | Advanced
```

with a direction-first surface:

```text
Phone → PC | PC → Phone | Advanced
```

The first two tabs are directions, not roles.

Remove:
- `relay-role-index`
- Emit / Receive / Both combobox

Display a read-only effective role:
- Phone → PC: `Listening / Host`
- PC → Phone: `Sending / Emitter`

### Desktop config

Replace `relay_role` with `relay_direction`.

Suggested serialized values:

```text
mobile_to_desktop
desktop_to_mobile
```

### Desktop bridge

Do not pass configurable `RelayRoles`.

Derive:

```rust
fn desktop_roles(direction: AudioDirection) -> RelayRoles {
    match direction {
        AudioDirection::MobileToDesktop => RelayRoles::receive_only(),
        AudioDirection::DesktopToMobile => RelayRoles::emit_only(),
    }
}
```

Host start in Mobile → PC must be receive-only.
Desktop client connect in PC → Mobile must be emit-only.

## Hot role switching / conflict resolution

A local mutex prevents two local audio engines, but it does not resolve simultaneous remote host selection.

Add a trusted control message:

```text
DirectionOffer {
  generation: u64,
  direction: AudioDirection,
  device_id: String
}

DirectionAck {
  generation: u64,
  direction: AudioDirection
}
```

Resolution order:
1. Higher `generation` wins.
2. Equal generation: lexicographically greater stable `device_id` wins.
3. Losing peer adopts the winning direction automatically.
4. Never negotiate `both`.

Two-phase switch:
1. User changes direction.
2. Sender increments generation and sends `DirectionOffer`.
3. Peer resolves and sends `DirectionAck`.
4. Both stop their old audio worker.
5. New host starts listening.
6. New emitter connects.
7. UI changes from `Switching…` to `Sending` / `Receiving`.

If the old connection disappears before ACK:
- persist the desired direction and generation;
- on trusted rediscovery, include the latest offer in reconnect;
- deterministic resolution still produces one host and one emitter.

## Invariants / tests

Add tests asserting:

- exactly one of emitter/host is active locally;
- no service ever starts with role `both`;
- Mobile → PC maps to Android emit-only + desktop receive-only;
- PC → Mobile maps to desktop emit-only + Android receive-only;
- repeated direction selection is idempotent;
- rapid A→B→A switching leaves only A active;
- simultaneous offers converge on the same direction;
- stale generations cannot reverse a newer direction;
- reconnect after process/network interruption resumes the persisted direction;
- legacy `both` config migrates to a deterministic one-way direction.

## Suggested PR split

1. `refactor(relay): replace role setting with audio direction`
2. `feat(relay): hot-switch direction without process restart`
3. `feat(relay): deterministic direction negotiation`
4. `feat(android): direction-first Material 3 redesign`
5. `feat(desktop): replace host/role UI with direction tabs`
