# Windows process-loopback capture

Windows process capture is implemented in
`pw-graph-backend::ProcessLoopbackSource`. It uses
`ActivateAudioInterfaceAsync` with the documented process-loopback activation
parameters and the `VAD\\Process_Loopback` virtual interface. The target PID
and an include/exclude process-tree mode are explicit; a source activation also
gets a monotonic generation so a restarted process cannot inherit a stale
route merely because Windows reused its numeric PID.

The activation is deliberately fail-closed. The `AUDIOCLIENT_ACTIVATION_PARAMS`
value is boxed, the `VT_BLOB` points into that box, and the owning
`PROPVARIANT`, completion handler, async operation, and result channel remain
live until the completion callback signals the activating thread. That thread
then calls `GetActivateResult` in its own COM apartment, so the
`IAudioClient` is never sent across the callback's MTA boundary. No stack blob
is passed to asynchronous Windows code.

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

The Windows relay reuses that same gate. Its Emitter source list includes
`application:<selector>` entries for active render sessions on QPWGraph Virtual
Output. The selector is a hash of the executable path; the current PID is
resolved from the live worker snapshot and is never persisted. Process-loopback
capture then feeds the relay's bounded PCM hand-off with the same negotiated
format as physical sources. If the session disappears or activation is
unsupported, the relay reports an error and does not substitute another
process. When used as a graph route instead, the same source feeds the router's
conversion, effects, meters, and route diagnostics.

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
activation should be run on a Windows 10 build 20348+ test machine with
`PW_GRAPH_TEST_PROCESS_LOOPBACK=1`; it requires a real target process and an
active audio session, so it is not run on headless CI.

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
