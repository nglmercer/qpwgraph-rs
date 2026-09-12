# QPWGraph Recorder Implementation Plan

## Goal

Add a first-class audio **Recorder node** to `qpwgraph-rs`.

The recorder should:

* Appear as a node in the audio graph.
* Accept audio connections from compatible sources.
* Record one or multiple sources.
* Support Linux/PipeWire and Windows/Core Audio.
* Support Windows application capture when process-loopback is available.
* Write recordings without blocking realtime audio threads.
* Default to asking where to save after recording stops.
* Remember the previously selected recording directory.
* Optionally support automatic saving to a configured folder.
* Preserve unfinished recordings when the user cancels the Save dialog or the application crashes.

The first supported file format should be **WAV 32-bit float**.

---

# 1. Existing Architecture to Reuse

Do not build an independent audio subsystem.

Reuse the existing architecture:

```text
crates/pw-graph-core
    graph types / nodes / ports

crates/pw-graph-backend
    backend APIs
    PipeWire backend
    Windows audio backend

crates/pw-graph-backend/src/router
    platform-neutral PCM router
    AudioSource
    AudioSink
    RingSink
    RingSinkDrain
    mixing
    resampling
    effects
    meters

crates/pw-graph-backend/src/windows
    WASAPI
    process loopback
    ProcessCaptureManager

crates/pw-graph-config
    persistent application settings

crates/pw-graph-slint
    UI
    native file dialogs through rfd
```

The Windows router already owns PCM for routes and supports arbitrary `AudioSink` implementations.

The recorder should therefore behave primarily as another audio destination.

---

# 2. Desired User Experience

## Creating a recorder

Provide an action such as:

```text
Add → Recorder
```

or:

```text
+ Recorder
```

The graph should display:

```text
┌────────────────────┐
│ Recorder           │
│                    │
│ ○ input L          │
│ ○ input R          │
│                    │
│ ● 00:00:00         │
│ [ Record ]         │
└────────────────────┘
```

Possible states:

```text
Idle
Armed
Recording
Stopping
Unsaved
Saved
Error
```

Minimum implementation only needs:

```text
Idle
Recording
Unsaved
Error
```

---

# 3. Graph Semantics

Add:

```rust
NodeType::Recorder
```

A recorder node is a graph destination.

Example:

```text
Microphone ──────────────┐
                         │
Browser ─────────────────┼──> Recorder
                         │
Speaker Monitor ─────────┘
```

Multiple connections should mix naturally.

Do not implement separate mixer logic inside Recorder if the existing router can mix sources into one sink.

The recorder should normally expose one logical audio input.

The UI may display stereo channel ports where required by the backend:

```text
record_FL
record_FR
```

but internal recording should operate on negotiated audio frames rather than relying on port names.

---

# 4. Important Windows Semantics

Do not make ordinary Windows application sessions generally routable just to support recording.

Currently, these are different operations:

```text
Application → Speaker
```

means:

```text
reroute application
```

while:

```text
Application → Recorder
```

means:

```text
read-only process capture
```

These must remain semantically different.

Introduce a destination-aware connection capability.

Example API:

```rust
pub enum ConnectionSupport {
    Route,
    CaptureOnly,
    Unsupported,
}
```

Possible API:

```rust
fn connection_support(
    &self,
    output: PortId,
    input: PortId,
) -> ConnectionSupport;
```

Expected Windows behavior:

```text
Microphone endpoint → Recorder
    Route / Capture

Speaker monitor → Recorder
    Route / Capture

QPWGraph-owned routed source → Recorder
    Route

Application session → Recorder
    CaptureOnly

Application session → normal playback endpoint
    Unsupported unless existing virtualization/routing path supports it

Application session → effect
    Unsupported unless qpwgraph owns/isolate the route
```

Do not simply change:

```rust
node_supports_routing()
```

to return `true` for Windows application sessions.

That would incorrectly expose unsupported application rerouting.

---

# 5. Recorder Backend API

Add a separate recorder contract instead of overloading unrelated graph APIs.

Suggested types:

```rust
pub type RecorderId = u64;

pub enum RecorderState {
    Idle,
    Recording,
    Unsaved,
    Error,
}

pub struct RecorderStatus {
    pub id: RecorderId,
    pub state: RecorderState,
    pub elapsed_frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub temporary_path: Option<PathBuf>,
    pub final_path: Option<PathBuf>,
    pub dropped_frames: u64,
    pub error: Option<String>,
}
```

Suggested trait:

```rust
pub trait RecorderDriver {
    fn supports_recorders(&self) -> bool;

    fn create_recorder(
        &mut self,
        request: RecorderCreateRequest,
    ) -> BackendResult<RecorderInstance>;

    fn remove_recorder(
        &mut self,
        id: RecorderId,
    ) -> BackendResult<()>;

    fn start_recording(
        &mut self,
        id: RecorderId,
    ) -> BackendResult<()>;

    fn stop_recording(
        &mut self,
        id: RecorderId,
    ) -> BackendResult<RecorderResult>;

    fn recorder_status(
        &self,
        id: RecorderId,
    ) -> BackendResult<RecorderStatus>;
}
```

Keep recording lifecycle separate from topology lifecycle.

---

# 6. Realtime Architecture

Never perform filesystem writes directly from:

* PipeWire callbacks
* WASAPI callbacks
* `RouterCore::process`
* any realtime audio callback

Use a bounded ring.

Architecture:

```text
Audio source
    │
    ▼
Router / PipeWire callback
    │
    ▼
RingSink / bounded queue
    │
    ▼
Recorder writer thread
    │
    ▼
temporary .wav.part file
```

The realtime side should only:

```text
copy PCM into bounded memory
update atomic diagnostics
return immediately
```

The writer thread should:

```text
pull PCM
encode/write WAV
flush/finalize outside realtime path
handle filesystem errors
```

---

# 7. Reuse Existing Router Ring Infrastructure

The existing router already has:

```rust
RingSink
RingSinkDrain
ring_sink(...)
```

Use it.

Recommended flow:

```rust
let (sink, drain) = ring_sink(format, capacity_frames);
```

Then:

```text
Router owns:
    RingSink

Recorder writer owns:
    RingSinkDrain
```

Avoid introducing another unbounded channel.

Recommended capacity:

```text
100–500 ms
```

Initially choose something conservative, for example:

```text
250 ms
```

The queue must remain bounded.

If the writer cannot keep up:

```text
increment dropped_frames
surface recorder warning
never allow memory growth without bound
```

---

# 8. Windows Device Recording

For physical capture devices:

```text
WASAPI capture
    ↓
existing router source
    ↓
Recorder sink
```

For playback-device recording:

```text
WASAPI render loopback
    ↓
existing router source
    ↓
Recorder sink
```

No Windows kernel driver should be required for these cases.

The optional virtual-audio driver is unrelated to writing qpwgraph-owned PCM to a local file.

---

# 9. Windows Per-Application Recording

Use the existing process-loopback infrastructure.

Do not create an entirely independent process-loopback implementation.

Current relevant code:

```text
crates/pw-graph-backend/src/windows/process_loopback.rs
crates/pw-graph-backend/src/windows/process_capture.rs
```

Extend:

```rust
ProcessCaptureConsumer
```

with:

```rust
Recorder(RecorderId)
```

Example:

```rust
pub enum ProcessCaptureConsumer {
    Meter(NodeId),
    Relay,
    OwnedRoute,
    Recorder(RecorderId),
    Diagnostics,
}
```

Long term, one process-loopback stream should be able to fan out to:

```text
Meter
Relay
Recorder
Diagnostics
```

without opening duplicate Windows process-loopback activations.

Important:

Windows process-loopback may reject or behave poorly with duplicate simultaneous activations for the same process.

Prefer shared capture ownership.

---

# 10. Windows Version Limitation

Per-application process-loopback should be runtime capability-gated.

Do not report it as universally supported.

Behavior:

```text
Windows version supports process loopback:
    Application → Recorder available

Windows version does not support process loopback:
    Application → Recorder disabled
```

Provide a useful explanation:

```text
Recording an individual application is unavailable on this version of Windows.
You can still record an input device or playback-device monitor.
```

Do not silently fall back to recording the entire system mix.

Recording a different source than the one selected is unacceptable.

---

# 11. Linux / PipeWire Recorder

Implement Recorder as a normal PipeWire capture destination/node.

Preferred behavior:

```text
Source node
    ↓
PipeWire link
    ↓
Recorder capture stream
    ↓
bounded queue
    ↓
writer thread
```

The recorder's graph representation should make the connection visible like other graph links.

Where possible, keep recorder semantics consistent between Linux and Windows.

---

# 12. Recording File Lifecycle

Do not wait until recording ends before writing audio.

Do not buffer an entire recording in RAM.

When recording begins:

```text
create temporary file
write audio continuously
```

Suggested path:

```text
<app-data>/recordings/pending/
    recording-<uuid>.wav.part
```

Example:

```text
recording-c6c8c056.wav.part
```

When Stop is pressed:

```text
stop accepting PCM
drain remaining queued PCM
finalize WAV header
close writer
mark recording Unsaved
open Save dialog
```

After successful save:

```text
move/copy temporary file to selected path
mark Saved
delete temporary file if copied
```

---

# 13. Never Lose Recording on Save Dialog Cancel

If the user presses Cancel in the save dialog:

Do not delete the recording.

State becomes:

```text
Unsaved
```

Show:

```text
Recording finished — not saved

[ Save… ]
[ Discard ]
```

The recording should remain in the application's pending/recovery directory.

Only delete it after explicit:

```text
Discard
```

or after a successful final save.

---

# 14. Crash Recovery

At application startup inspect:

```text
recordings/pending/
```

for recoverable files.

If valid unfinished/finalized recordings exist, show:

```text
Recovered recordings

Recording from 2026-09-11 20:14
03:42
18.4 MB

[ Save… ]
[ Discard ]
```

Prefer making `.part` files independently recoverable.

For WAV this means either:

1. periodically maintaining a valid WAV header, or
2. repairing the header using file length during recovery.

Avoid creating recordings that become completely useless after an application crash.

---

# 15. Initial File Format

Implement first:

```text
WAV
32-bit IEEE float
interleaved PCM
```

Reason:

The router already uses:

```rust
f32
```

audio.

This avoids an additional sample conversion on the writer path.

Suggested dependency:

```toml
hound = "..."
```

or a very small internal WAV writer if dependency minimization is preferred.

The writer must finalize:

```text
RIFF length
data chunk length
```

after recording stops.

---

# 16. Future Formats

Do not implement these in the first pass unless trivial:

```text
WAV 24-bit PCM
FLAC
Opus
AAC
MP3
```

Recommended future priority:

```text
1. WAV float32
2. FLAC
3. WAV PCM24
```

Lossy formats can come later.

---

# 17. Large WAV Files

Classic RIFF WAV has practical size limitations around 4 GiB.

The first implementation can detect approaching the limit.

Possible behavior:

```text
if predicted file size approaches WAV limit:
    stop recording with clear warning
```

Future implementation:

```text
RF64
```

or:

```text
automatic file splitting
```

Example:

```text
Recording 001.wav
Recording 002.wav
```

Do not silently overflow WAV chunk sizes.

---

# 18. Default Save UX

Default behavior:

```text
Ask where to save after recording
```

Flow:

```text
Record
↓
temporary recording is written
↓
Stop
↓
native Save dialog
↓
user chooses destination
```

Use the existing:

```rust
rfd::FileDialog
```

pattern already used by patchbay files.

Example:

```rust
FileDialog::new()
    .set_directory(last_recording_dir)
    .set_file_name(default_name)
    .add_filter("WAV audio", &["wav"])
    .save_file();
```

---

# 19. Remember Last Recording Folder

Add configuration:

```rust
pub recording_dir: Option<PathBuf>,
```

After a successful save:

```rust
config.recording_dir = final_path.parent().map(PathBuf::from);
```

The next Save dialog opens there.

This should be enabled by default.

There is no need for a special checkbox inside the native file picker.

---

# 20. Optional Auto-Save Mode

Add:

```rust
pub enum RecordingSaveMode {
    AskOnStop,
    AutoSave,
}
```

Config representation may simply use a string:

```toml
recording_save_mode = "ask"
```

or:

```toml
recording_save_mode = "auto"
```

For automatic mode require:

```rust
recording_dir: Some(...)
```

Behavior:

```text
Record
↓
Stop
↓
Recording automatically finalized to configured folder
```

If the folder is unavailable:

```text
fall back to Unsaved
keep temporary file
display error
```

Do not discard the recording.

---

# 21. Filename Generation

Default template:

```text
Recording YYYY-MM-DD HH-MM-SS.wav
```

Example:

```text
Recording 2026-09-11 20-18-42.wav
```

Avoid characters invalid on Windows.

Do not use:

```text
:
*
?
"
<
>
|
```

If the file already exists:

```text
Recording 2026-09-11 20-18-42 (2).wav
```

Never overwrite an existing recording silently.

---

# 22. Suggested Configuration Fields

Add to `AppConfig`:

```rust
pub recording_dir: Option<PathBuf>,

pub recording_save_mode: String,

pub recording_filename_template: String,

pub recording_format: String,
```

Defaults:

```rust
recording_dir = None

recording_save_mode = "ask"

recording_filename_template = "Recording {date} {time}"

recording_format = "wav-f32"
```

Do not persist active recordings as if they were normal configuration.

Runtime recording sessions belong to application/backend state.

---

# 23. UI Controls

Recorder node:

```text
Recorder

Input: Stereo
00:03:21

● Recording

[ Stop ]
```

Idle:

```text
Recorder

Input: Stereo

[ Record ]
```

Unsaved:

```text
Recorder

03:21 recorded
Not saved

[ Save… ]
[ Discard ]
```

Possible status indicators:

```text
● red      Recording
● orange   Unsaved
● green    Saved
⚠          Dropped audio / Error
```

---

# 24. Recorder Preferences

Add a Recorder section to Preferences.

Suggested controls:

```text
Recording format
    WAV 32-bit float

After recording
    ○ Ask where to save
    ○ Automatically save

Recording folder
    C:\Users\...\Music\Recordings
    [ Choose… ]

Remember last folder
    enabled implicitly for Ask mode
```

Avoid too many options in the initial implementation.

---

# 25. Stop Semantics

`Stop` should be asynchronous from the UI perspective.

Do not block the UI while:

```text
draining queue
finalizing WAV
flushing file
```

Possible state:

```text
Stopping…
```

Writer thread reports completion back to control thread.

Then show Save dialog.

---

# 26. Application Exit While Recording

If application shutdown is requested during recording:

Preferred behavior:

```text
Recording is in progress.

[ Stop and Quit ]
[ Cancel ]
```

If implementing confirmation is difficult initially:

At minimum:

```text
stop recording
finalize temporary file
leave it recoverable
then exit
```

Never intentionally truncate/delete the recording during normal application shutdown.

---

# 27. Recorder Diagnostics

Expose:

```rust
frames_written
dropped_frames
queue_depth
queue_capacity
sample_rate
channels
writer_state
file_bytes
last_error
```

Example diagnostic:

```text
Recorder 1
State: Recording
Format: 48000 Hz / 2 ch / float32
Frames written: 18,432,000
Dropped frames: 0
Queue: 1920 / 12000 frames
File: .../recording-uuid.wav.part
Writer: Active
```

This should not require touching the realtime thread.

Use atomics / control-thread snapshots consistent with existing router diagnostics.

---

# 28. Error Handling

Handle explicitly:

```text
disk full
permission denied
folder removed
file handle lost
writer thread failure
queue overrun
source disappears
process exits
unsupported Windows process capture
```

If disk writing fails during recording:

```text
stop accepting new audio
mark recorder Error
preserve file already written
show exact error
```

Do not continue displaying:

```text
Recording
```

when nothing is being written.

---

# 29. Source Disappearance

If the source disappears during recording:

For a single-source recording:

```text
insert silence or stop depending on existing router semantics
```

Preferred behavior:

```text
keep timeline continuous
count discontinuity
show warning
```

For multi-source recording:

```text
remaining sources continue
missing source contributes silence
```

Reuse router behavior where possible.

---

# 30. Testing Strategy

## Platform-neutral tests

Add tests with:

```rust
BufferSource
RingSink
RecorderWriter
```

Test:

```text
known f32 samples → WAV → decode → samples match
```

Test stereo ordering.

Test duration.

Test mixed sources.

Test sample-rate conversion path.

Test queue overflow.

Test writer error.

---

# 31. WAV Tests

Generate:

```text
1 kHz tone
48 kHz
2 channels
1 second
```

Verify:

```text
sample_rate == 48000
channels == 2
frames == 48000
duration == 1 second
peak approximately expected amplitude
```

Verify header finalization.

Verify `.part` recovery.

---

# 32. Windows Tests

Where available test:

```text
microphone → recorder
render loopback → recorder
process loopback → recorder
```

Tests requiring actual devices should remain opt-in.

Example environment variable:

```text
PW_GRAPH_TEST_RECORDER=1
```

Headless CI should skip native audio-device tests safely.

---

# 33. Linux Tests

Where PipeWire is available:

```text
test tone / source
    ↓
Recorder
```

Validate output.

Keep live-session tests opt-in if they alter the user's graph.

---

# 34. Process Capture Tests

Test that the same process capture can logically have multiple consumers:

```text
Meter
Recorder
```

Expected:

```text
one underlying capture identity
two consumers
```

Also test:

```text
remove Recorder
```

while Meter remains.

The capture must stay active until its last consumer is removed.

---

# 35. Implementation Order

## Phase 1 — File writer

Implement platform-neutral:

```text
RecorderWriter
WAV float32
temporary file
finalization
status/diagnostics
```

No UI yet.

---

## Phase 2 — Router recorder sink

Implement:

```text
RingSink
writer drain thread
router sink registration
```

Test entirely with in-memory sources.

---

## Phase 3 — Recorder backend API

Add:

```text
RecorderDriver
RecorderInstance
RecorderState
RecorderStatus
```

Implement in demo/in-memory backend first where useful.

---

## Phase 4 — Windows device recording

Support:

```text
capture endpoint → Recorder
playback monitor → Recorder
existing qpwgraph-owned route → Recorder
```

---

## Phase 5 — Windows application recording

Extend:

```text
ProcessCaptureManager
ProcessCaptureConsumer
```

with Recorder consumers.

Implement:

```text
Application → Recorder
```

as `CaptureOnly`.

Do not enable general application rewiring.

---

## Phase 6 — PipeWire recording

Implement PipeWire recorder stream/node and connect it to the same writer infrastructure.

---

## Phase 7 — Graph/UI

Add:

```text
NodeType::Recorder
Create Recorder
Record
Stop
elapsed time
Save
Discard
```

---

## Phase 8 — Save dialog and config

Add:

```text
recording_dir
recording_save_mode
recording_format
filename template
```

Reuse `rfd::FileDialog`.

---

## Phase 9 — Recovery

Add:

```text
pending recording directory
startup recovery detection
Save / Discard UI
```

---

# 36. Files Likely to Change

Expected areas:

```text
crates/pw-graph-core/src/lib.rs

crates/pw-graph-backend/src/api.rs
crates/pw-graph-backend/src/lib.rs

crates/pw-graph-backend/src/router/
    endpoints.rs
    engine.rs
    mod.rs
    tests.rs
    recorder.rs          # suggested new module

crates/pw-graph-backend/src/windows/
    process_capture.rs
    process_loopback.rs
    routing-related files
    recorder.rs          # optional platform adapter

crates/pw-graph-backend/src/pipewire/
    recorder.rs

crates/pw-graph-config/src/lib.rs

crates/pw-graph-slint/src/source.rs

crates/pw-graph-slint/src/bridge/
    callbacks.rs
    actions.rs
    config.rs
    recorder.rs          # suggested

Slint UI files
i18n strings
docs/platform-parity.md
docs/features.md
```

Do not force all recorder implementation into `source.rs` or UI bridge files.

Keep audio/file lifecycle in backend/domain code.

---

# 37. Architectural Rules

The implementation must preserve these rules.

## Rule 1

No filesystem I/O on realtime audio threads.

## Rule 2

No unbounded audio queues.

## Rule 3

Never silently discard a completed recording.

## Rule 4

Never silently record a different source if the requested source is unavailable.

## Rule 5

Windows application recording is capture, not arbitrary application rerouting.

## Rule 6

Do not require the optional Windows virtual-audio driver merely to record qpwgraph-owned PCM.

## Rule 7

Recorder errors must be visible.

## Rule 8

Canceling Save must preserve the recording.

## Rule 9

Source identity must remain stable enough that Windows PID reuse cannot record an unrelated application.

Reuse the existing stable process selector/generation logic.

## Rule 10

Tests should cover the platform-neutral recorder without requiring real audio devices.

---

# 38. Definition of Done

The first recorder release is complete when all of the following work:

```text
Linux:
    microphone/source → Recorder → WAV

Windows:
    microphone → Recorder → WAV

Windows:
    playback device monitor → Recorder → WAV

Supported Windows versions:
    application → Recorder → WAV

Both platforms:
    multiple compatible sources → Recorder
    resulting file contains mixed audio

UI:
    create recorder
    connect source
    start
    elapsed time updates
    stop

After Stop:
    native Save dialog appears

Save dialog:
    opens in previously used directory

Cancel Save:
    recording remains recoverable

Save:
    final WAV exists at selected path

Crash/startup:
    pending recording can be recovered

Realtime:
    no filesystem writes occur on audio callback/router thread

Tests:
    deterministic WAV writer tests pass
    router recorder tests pass
```

---

# 39. Recommended First PR Scope

Keep the first PR relatively narrow.

Implement:

```text
Recorder writer
WAV float32
RingSink integration
Recorder backend contract
one Recorder graph node
device/router-source recording
Stop → Save As
remember last directory
Cancel → Unsaved
```

Leave for later PRs:

```text
FLAC
RF64
advanced filename templates
multiple recorder instances if complexity is high
automatic recording
scheduled recording
recording history
waveform preview
markers
editing
pause/resume
```

Per-application Windows recording may be included in the first PR if the existing process-capture manager can be extended cleanly without destabilizing relay/meter behavior.

Otherwise make it the second PR.

---

# 40. Final Design Principle

Recorder should be treated as:

```text
a normal graph destination
+
a bounded asynchronous file writer
```

not as:

```text
a special UI command that directly opens and captures an audio device
```

Keeping Recorder inside the graph model preserves the strongest part of `qpwgraph-rs`: users can see exactly what audio is being recorded, connect effects before the recorder, mix sources deliberately, and use the same mental model on Linux and Windows.
