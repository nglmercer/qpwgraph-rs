# Windows driver development

The driver project is intentionally separate from the normal workspace:

```text
drivers/windows-audio/
  core/       allocation-free ring and state-machine units
  driver/     no_std KMDF/ACX cdylib
  package/    INF/INX and package notes
  tests/smoke/ endpoint smoke-test entry point
  xtask/      WDK environment/package checks
```

Run the commands from an eWDK developer prompt. The regular Windows SDK is not
enough: `wdk-sys` needs WDK kernel headers and libraries, and ACX bindings need
the ACX headers from that installation. The build script intentionally fails
with a missing-WDK diagnostic instead of falling back to a user-mode DLL.

Before enabling an endpoint implementation, prove the Stage-0 checklist on a
disposable Windows VM: load, enumerate, WASAPI open/start/stop, disable/enable,
unload, and Driver Verifier. Then add the render/capture ring, identity
migration, the Relay pair, and only afterward package the full installer.

The kernel surface must stay small: standard audio streams are preferred over
custom privileged IOCTLs, every buffer/format is bounded and validated, no
panic crosses FFI, and each unsafe wrapper documents its invariant. Test
signing is development-only; release signing and HLK results are release
artifacts, not assumptions.
