# QPWGraph Windows virtual-audio package

This is a nested Rust workspace so normal application builds never acquire a
WDK dependency. Use an eWDK prompt with KMDF 1.33 and LLVM available:

```powershell
cargo test -p qpwgraph-audio-core
cargo run -p qpwgraph-audio-xtask
cargo make --cwd drivers/windows-audio
```

The current driver entry point intentionally returns `STATUS_NOT_SUPPORTED`.
It must not be installed until the ACX device and circuit creation path is
implemented. This fail-closed state prevents an endpoint-less development
binary from being confused with a successful Stage-0 driver.
The installer checks `manifest.json` and refuses packages whose
`implementation_status` is not `ready`.

The install metadata is `package/qpwgraph-audio.inx`; the copy beside the
driver source is retained as a template for driver-local builds.

The package must never set a Windows default audio device. Test signing is for
development machines only; public packages require Microsoft signing and
Secure Boot validation.

`install.ps1` and `uninstall.ps1` are intentionally thin PnPUtil wrappers. The
uninstaller requires the exact published `oemNN.inf` name and supports
`-WhatIf`; it never searches for or removes an unrelated driver package.
