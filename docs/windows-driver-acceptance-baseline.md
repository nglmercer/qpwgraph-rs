# Windows driver acceptance baseline

This is a development-package baseline captured on 2026-09-06. It records
what passed on the available Windows test machine; it is not evidence for a
current WDK build, Microsoft signing, HLK, Secure Boot, or final release.
Regenerate it after installing a package produced from the candidate commit.

## September 12 recheck after Test Mode reboot

The installed SYS still hashes to
`C097A4C14E4EB528A409D038B8CF9E581879CD3D119A8A54CAE220E59A91B3AD`,
matching this baseline. It differs from the Rust candidate staged after
commit `61542ff` (before the packet-layout change), whose SYS hashes to
`A0B3EAB8B1A9D3DF2834F47E54D2B6793E29E5EC398A36F29367A48DAB8B7172`.

Commands run from the repository root with the existing smoke executable:

```powershell
Get-FileHash C:/Windows/System32/drivers/qpwgraph_audio.sys
& ./drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe --verify-roles
& ./drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe --verify-cables --duration-ms 1500
```

Both probes exited 0. Four roles enumerated; app and relay target peaks were
0.208038 and 0.250031 respectively. Each other cable stayed at peak 0;
stopped-render checks captured 24,000 silent frames on each endpoint.
These results revalidate the installed baseline only. They do not validate
the current Rust candidate or its cleanup and packet-layout changes.

The candidate's single-packet allocator now returns a whole-page audio
extent at offset zero, including the rounded size in its runtime timing
state. Its two-packet layout retains the requested audio size. This follows
the [ACX buffer mapping requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-streaming#stream-resource-allocation).
Live timer-driven consumption and position accuracy still need verification;
allocation tests alone cannot prove those streaming requirements.

## Machine and package

- OS: Windows 10 Pro, build 19045
- Package: drivers/windows-audio/target/qpwgraph-audio-package
- Manifest status: ready, driver version 0.1.0
- Service: qpwgraph_audio
- Package manifest SHA-256:
  1F43473D632F515BA2C46DE9B8E81D01F2AF9CAC657A781635D43B9654CDB6AD
- SYS SHA-256:
  C097A4C14E4EB528A409D038B8CF9E581879CD3D119A8A54CAE220E59A91B3AD
- INF SHA-256:
  2833E3558D2B54B0637701DD3E6F9CA2DF012303775BE4D1B82925882DCEEFDA
- CAT SHA-256:
  E1C27744BC48F4F8C7266751F58972A6ADE30E3557AE1ACC940AF1D122C679C8
- SYS/CAT Authenticode: Valid, signer CN=QPWGraph Audio Test

The test certificate is development-only. It must not be treated as the
Microsoft production publisher.

## Endpoint and cable smoke

The smoke probe verified all four provider-owned roles with exit code 0:

- app-render: QPWGraph Virtual Audio development speakers
- app-monitor: QPWGraph Virtual Audio development microphone
- relay-render: QPWGraph Virtual Audio development speakers
- relay-capture: QPWGraph Virtual Audio development microphone

The two-cable verification passed with exit code 0:

- app cable: 1 kHz target peak about 0.208; relay capture remained silent
- relay cable: 2 kHz target peak about 0.250; app monitor remained silent
- stopped-render silence passed for both cables

## User-mode application-policy smoke

On the same Windows 10 build, the opt-in automatic application-route probe
passed against the installed development endpoints. An unpackaged helper
opened on its ordinary default output, all three render roles moved to
`app-render`, a replacement PID was rebound, and the original role-specific
values were restored when the rule was removed. The shutdown-specific probe
also passed: with the replacement lease still active, dropping the backend
restored all three roles before worker shutdown. These probes validate the
user-mode policy boundary and ownership logic; they are not evidence that a
new Rust ACX candidate package was built or installed.

## Stress baseline

The explicit driver stress script completed with exit code 0:

- 100 app render/capture round-trip cycles
- 100 relay render/capture round-trip cycles
- 100 two-cable isolation cycles
- AudioSrv restart: not run in this baseline
- device disable/enable: not run in this baseline

The stress run was against the installed development package. Driver Verifier,
event-log review, crash recovery, lifecycle, client matrix, and production
signing remain separate gates.
