# Windows process-loopback capture

Windows process capture is implemented in
`pw-graph-backend::ProcessLoopbackSource`. It uses
`ActivateAudioInterfaceAsync` with the documented process-loopback activation
parameters and the `VAD\\Process_Loopback` virtual interface. The target PID
and an include/exclude process-tree mode are explicit; a source activation also
gets a monotonic generation so a restarted process cannot inherit a stale
route merely because Windows reused its numeric PID.

The activation is deliberately fail-closed. The
`AUDIOCLIENT_ACTIVATION_PARAMS` value is allocated with the COM task allocator
because `PROPVARIANT` releases `VT_BLOB` storage with `CoTaskMemFree`. The
owning `PROPVARIANT`, completion handler, async operation, and result channel
remain live until the completion callback signals the activating thread. That
thread then calls `GetActivateResult` in its own COM apartment, so the
`IAudioClient` is never sent across the callback's MTA boundary. No stack blob
or Rust-heap pointer is passed to asynchronous Windows code.

After activation, the source is initialized as shared-mode float32 loopback
audio and feeds the same bounded `RingSource` used by physical WASAPI
endpoints. Capture callbacks do not allocate, lock, format, or grow a queue;
silence packets are synthesized and ring overflow is dropped and observable.
The router therefore supplies the existing channel conversion, effects, true
RMS meter, gain, and route diagnostics.

Process loopback is capability-detected by trying the operation and handling
its HRESULT. A missing or restricted API is a capability transition, not an
application-startup failure. Applications on ordinary endpoints remain
observed, immutable session links. A session becomes routable only after it is
already attached to `QPWGraph Virtual Output`, which is the proof that the
original dry path has been isolated.

The Windows relay reuses the same process-loopback implementation, but its
Emitter source list is intentionally independent of virtual-output isolation:
it includes `application:<selector>` entries for active render sessions on
ordinary endpoints as well as isolated sessions. The selector is a hash of the
executable path; the current PID is resolved from the live worker snapshot and
is never persisted. Process-loopback capture then feeds the relay's bounded
PCM hand-off with the same negotiated format as physical sources. If the
session disappears or activation is unsupported, the relay reports an error
and does not substitute another process. When used as a graph route instead,
the same source feeds the router's conversion, effects, meters, and route
diagnostics.

The persisted Windows policy reserves an explicit opt-in switch for frontends
that want to expose process capture:

```toml
[windows]
enable_process_loopback = true
```

The low-level backend still fails closed on unsupported activation and only
offers a process session as a route after the app is already on QPWGraph
Virtual Output; automatic policy restoration is not implied by this setting.

The low-level source is covered by layout/lifetime unit tests. End-to-end
activation is an opt-in test on a Windows 10 build 20348+ host with
`PW_GRAPH_TEST_PROCESS_LOOPBACK=1`; it requires a real target process and an
active audio session, so it is not run on headless CI. The deterministic helper
path passed on the local Windows 10 host on 2026-09-05.

The same helper can exercise the complete backend meter path, including a
true per-process RMS reading without a virtual driver:

```powershell
$env:PW_GRAPH_TEST_PROCESS_RMS = '1'
cargo test -p windows-audio-test-tone --test process_loopback --locked -- --nocapture
```

The policy/lifetime smoke test exercises `always`, `on-demand`, and `off`:

```powershell
$env:PW_GRAPH_TEST_PROCESS_POLICY = '1'
cargo test -p windows-audio-test-tone --test process_loopback --locked -- --nocapture
```

The worker also watches the target process lifetime and reports a lost stream
when the target exits instead of leaving a silent capture alive:

```powershell
$env:PW_GRAPH_TEST_PROCESS_EXIT = '1'
cargo test -p windows-audio-test-tone --test process_loopback --locked -- --nocapture
```

The opt-in relay smoke test selects the helper as an ordinary application
source, starts the direct Emitter worker without the virtual driver, observes
the worker stop after the target exits, and verifies that the same stable
selector binds after a new PID appears:

```powershell
$env:PW_GRAPH_TEST_RELAY_APPLICATION = '1'
cargo test -p windows-audio-test-tone --features relay-tests --test process_loopback --locked -- --nocapture
```

Set `PW_GRAPH_TEST_RELAY_APPLICATION_SESSION=1` as well to run the local
authenticated host/client variant; it verifies that target exit stops only the
capture worker while the relay control session remains active.

The repository includes a deterministic target-process helper. Build and run
it for a manual smoke test (the first line prints the PID to capture):

```powershell
cargo run -p windows-audio-test-tone --release -- --duration-ms 30000 --frequency 1000 --amplitude 0.25
```

Use Windows Volume Mixer to assign that PID to `QPWGraph Virtual Output` when
the optional driver is installed. A process-loopback route can then be
connected to a physical destination; stopping the helper exercises the
source-loss path without relying on a particular media application.

For support reports, `WindowsAudioDriver::windows_audio_report()` exports the
current virtual-driver health, node capabilities, and route counters without
including raw audio or endpoint property blobs.
