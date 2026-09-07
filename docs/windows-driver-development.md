# Windows driver development

The driver project is intentionally separate from the normal workspace:

```text
drivers/windows-audio/
  core/       allocation-free ring and state-machine units
  driver/     no_std KMDF/ACX cdylib
  package/    INF/INX, release gates, and package notes
  tests/smoke/ endpoint smoke-test entry point
  xtask/      WDK environment/package checks
```

Run the commands from an eWDK developer prompt. The regular Windows SDK is not
enough: `wdk-sys` needs WDK kernel headers and libraries, and ACX bindings need
the ACX headers from that installation. The build script intentionally fails
with a missing-WDK diagnostic instead of falling back to a user-mode DLL.

The current ACX implementation is an opt-in eWDK target. The user-mode
transport and kernel callback runtime are Rust-owned, while the public package
remains fail-closed until the live validation gates are complete. Before
publishing, prove the Stage-0 checklist on a disposable Windows VM: load, enumerate, WASAPI
open/start/stop, disable/enable, unload, and Driver Verifier. Then run the
HLK, Secure Boot, signing, lifecycle, and ordinary-client gates.

The last available development-package snapshot is recorded in
[windows-driver-acceptance-baseline.md](windows-driver-acceptance-baseline.md).
It is explicitly a test-signed baseline and does not prove the current source
candidate or any production release gate.

The kernel surface must stay small: standard audio streams are preferred over
custom privileged IOCTLs, every buffer/format is bounded and validated, no
panic crosses FFI, and each unsafe wrapper documents its invariant. Test
signing is development-only; release signing and HLK results are release
artifacts, not assumptions. See [windows-driver-hlk.md](windows-driver-hlk.md)
and the package's read-only audit helpers for the evidence boundary.
