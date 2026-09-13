# Windows parity execution record

## Phase 0 baseline — September 13, 2026

BASE_COMMIT=918cccc29555aba7e1d4e39fa656482152198f67
WINDOWS_BUILD=Windows 10 Pro 22H2, 19045.6466
DRIVER_VERSION=11.8.28.7 (oem24.inf)
DRIVER_SOURCE=25ddbe6 (retained installed-candidate record)
PACKAGE_HASH_SYS=099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2
PACKAGE_HASH_INF=2A6CFF5CB0EB9E8CAA938FC8F6763F182D13628CE00A07C5E2E712C9DA918D7D
PACKAGE_HASH_CAT=5149E53234FC0E9C200DAC946E0D455D9C4FEFCDF2D3F79F24F5738C87CE77BB

The original main checkout was clean. Package hashes and the installed INF/version
were freshly read; the source-to-installed-binary relationship remains the retained
candidate record, not a newly built RC. No boot settings or installed packages were
changed. The staged manifest's product version 0.1.0 is not the installed INF version.

### Fresh baseline checks

- PASS: `cargo test -p pw-graph-backend --all-features --locked`.
- PASS: `cargo test --manifest-path drivers/windows-audio/Cargo.toml --workspace --locked`
  (22 core, 6 KS-probe, 10 smoke unit tests).
- PASS: driver xtask `--validate-package` and `--audit-toolchain`.
- PASS: mocked `tests/validation-workflows.ps1`, `tests/install-binding.ps1`,
  and `tests/release-audit.ps1` under `drivers/windows-audio`.
- OBSERVATION: `package/release-audit.ps1 -Json`; preparation/toolchain checks do
  not close external gates. Audit reports missing HLK and disabled Secure Boot.
- FAIL (pre-existing): `cargo fmt --all -- --check` in
  `crates/pw-graph-slint/src/bridge/icons.rs` test formatting.
- NOT RUN: physical no-driver portable smoke (the optional driver is installed).
  The environment-gated test returning early is not live portable acceptance.

Raw local logs: original checkout `target/parity-execution-20260913/`.
Historical live successes and failures remain in
`docs/windows-driver-candidate-acceptance.md` and its referenced evidence.

### Task classification

| Original task | Classification | Remaining work |
| --- | --- | --- |
| PR1 acceptance baseline | LANDED | Fresh baseline recorded here; candidate-specific evidence stays separate. |
| PR2 Rust ACX binding | LANDED | Preserve WDK-derived layouts. |
| PR3 device/circuit port | LANDED | Full lifecycle validation remains separate. |
| PR4 pins/formats/jacks | LANDED | Bounded live checks retained. |
| PR5 stream lifecycle port | LANDED | Leak freedom needs long-run/Verifier evidence. |
| PR6 RT callbacks | LANDED | Long-run counter/preroll validation remains. |
| PR7 EOS/Rust-only runtime | VALIDATION_ONLY | Bounded EOS and rollover units exist; extend long-run/preroll evidence. |
| PR8 driver live parity | VALIDATION_ONLY | Preserve passing direct KS cases; complete remaining runtime matrix. |
| PR9 private policy backend | IMPLEMENTATION_REMAINING | Review build allowlist against actual evidence; prove fail-closed fallback. |
| PR10 ownership | IMPLEMENTATION_REMAINING | Test manual override and partial-role restoration paths. |
| PR11 reconciliation | VALIDATION_ONLY | Restart/rebind units and live evidence exist; review uncovered failures. |
| PR12 Verifier tooling | IMPLEMENTATION_REMAINING | Existing scripts need evidence-integrity review; live gate is external. |
| PR13 lifecycle | VALIDATION_ONLY | Sleep/hibernate/reboot/install/upgrade remain open. |
| PR14 HLK preparation | LANDED | Review reproducibility; HLK execution is EXTERNAL_GATE. |
| PR15 Secure Boot tooling | LANDED | Read-only helper exists; separate configured host is EXTERNAL_GATE. |
| PR16 Microsoft signing preparation | LANDED | Review artifact identity; submission/signing is EXTERNAL_GATE. |
| PR17 full installer/workflow | IMPLEMENTATION_REMAINING | Review exact accepted-package binding and portable independence. |
| PR18 backend/destination recovery | IMPLEMENTATION_REMAINING | Test stale identity fallback and backend/relay recovery. |
| PR19 documentation | IMPLEMENTATION_REMAINING | Update after source and evidence stabilize. |
| Module-backed effects | LANDED | Explicitly unsupported on Windows; not silently accepted. |

### Execution boundaries

All Phase 1 worktrees start from BASE_COMMIT. Agents own disjoint files, commit
their changes, and never merge sibling branches. Coordinator owns this record,
shared interfaces, integration, and the pre-existing formatting correction.

EXTERNAL_GATES: real Driver Verifier run, missing lifecycle environments, HLK lab,
separate Secure Boot host, Microsoft submission and returned production signature,
full installer acceptance using the exact accepted returned package. They remain
open until exact-candidate evidence exists. Development Test Mode is preserved.
