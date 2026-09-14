# Windows Feature Parity — Parallel Multi-Agent Execution Plan

## Purpose

This execution plan replaces the old assumption that PR 1 through PR 19 must be completed serially.

The repository is already beyond several original milestones. Agents MUST inspect current source and current evidence before implementing anything.

The objective is:

```text
maximize parallel work
+
minimize overlapping file edits
+
merge only at explicit synchronization points
+
never claim a validation gate without evidence
```

Each agent gets one isolated worktree/branch.

No two implementation agents should own the same files during the same phase.

---

# PHASE 0 — BASELINE + WORK SPLIT

**Run first. One coordinator agent only.**

## Agent 0 — Baseline Coordinator

### Goal

Create the exact starting point all other agents will use.

### Tasks

1. Inspect current `main`.
2. Read:

   * `docs/WINDOWS_FEATURE_PARITY_PLAN.md`
   * candidate acceptance evidence
   * Windows driver documentation
   * current Windows backend implementation
3. Run the currently supported test/audit commands.
4. Record:

   * commit SHA
   * Windows build
   * driver version
   * package hash
   * current test results
5. Classify every parity task as:

```text
LANDED
VALIDATION_ONLY
IMPLEMENTATION_REMAINING
EXTERNAL_GATE
BLOCKED
```

6. Do NOT implement features.

### Output

Create a short handoff:

```text
BASE_COMMIT=
DRIVER_VERSION=
PACKAGE_HASH=

LANDED:
...

OPEN_IMPLEMENTATION:
...

OPEN_VALIDATION:
...

EXTERNAL_GATES:
...
```

### Merge checkpoint

All Phase 1 agents start from the same `BASE_COMMIT`.

---

# PHASE 1 — PARALLEL IMPLEMENTATION

All agents in this phase may work simultaneously.

They MUST NOT wait for each other unless an actual API dependency is discovered.

---

## Agent 1A — Application Routing Policy

### Scope

Automatic per-application Windows output switching only.

### Focus

Finish remaining application-routing gaps rather than rewriting the working implementation.

Priority work:

```text
manual user override behavior
unsupported-Windows-build fallback
fail-closed policy behavior
previous-route restoration
process replacement / PID rebind
destination-loss behavior related specifically to AppRoutePolicy
multi-build support detection
```

### Preserve

```text
AppRoutePolicy abstraction
ManualOnly fallback
default-disabled experimental policy where required
no guessed COM vtable slots
no PID persistence as application identity
portable/no-driver mode
```

### Suggested ownership

Primarily:

```text
crates/pw-graph-backend/src/windows/
```

Only files directly related to application route policy/reconciliation.

### Do not touch

```text
drivers/windows-audio/
packaging/windows/
Verifier scripts
HLK scripts
release signing scripts
```

### Done when

Tests cover success and fail-closed behavior and remaining live-routing gaps are either proven or explicitly documented as open.

---

## Agent 1B — Driver Lifecycle + Long-Run Audio Validation

### Scope

Rust ACX driver runtime validation.

### Focus

Remaining runtime/lifecycle gaps such as:

```text
long-running EOS behavior
preroll behavior
counter rollover assumptions
repeated stream lifecycle
sleep/resume
device disable/enable
AudioSrv restart
reboot where environment permits
repeated install/uninstall
repeated upgrade
```

Do not redo already-passing direct KS EOS/timing/lifecycle tests unless needed for regression testing.

### Ownership

```text
drivers/windows-audio/tests/
drivers/windows-audio/src/
```

Driver source changes are allowed only when a live or automated test demonstrates a real defect.

### Do not touch

```text
AppRoutePolicy
release installer
Microsoft signing pipeline
HLK documentation
```

### Done when

Every tested lifecycle case has retained evidence containing exact candidate identity and result.

---

## Agent 1C — Backend + Destination Recovery

### Scope

User-mode recovery outside AppRoutePolicy implementation.

### Focus

```text
qpwgraph backend crash/restart
physical destination disappears
physical destination returns
endpoint identity churn
relay recovery
stale destination handling
wrong-device prevention
```

### Important boundary

Agent 1A owns policy decisions.

Agent 1C owns backend/device recovery.

If an interface change between them is required, document the proposed interface instead of editing the other agent's files.

### Done when

Recovery tests prove that backend/device failures do not silently reconnect to an incorrect endpoint.

---

## Agent 1D — Driver Verifier Tooling

### Scope

Driver Verifier automation and evidence.

### Build

Or finish:

```text
enable-verifier.ps1
disable-verifier.ps1
run-driver-stress.ps1
collect-verifier-evidence.ps1
```

### Requirements

Verifier activation MUST require an explicit action.

Normal builds must never enable Verifier.

Evidence must identify:

```text
source commit
driver version
package hash
Windows build
Verifier configuration
test matrix
result
crash/dump information when applicable
```

### Do not modify

Driver runtime merely to make tooling convenient.

Runtime bugs discovered by this agent should be handed to Agent 1B.

---

## Agent 1E — Release / HLK / Secure-Boot Preparation

### Scope

Repository-side release tooling only.

This agent does NOT claim external validation succeeded.

### Prepare or verify

```text
HLK preparation helper
HLK manifest/evidence format
Secure Boot read-only audit
release-driver build
Dashboard submission preparation
returned-driver signature verification
```

### Important

Distinguish:

```text
PREPARATION
!=
LIVE VALIDATION
!=
MICROSOFT SIGNING
```

Never change Secure Boot/Test Mode automatically.

Never store Microsoft/EV credentials in the repository.

### Done when

The repository can reproducibly prepare and inspect all artifacts required by external release gates.

---

## Agent 1F — Windows Packaging / Installer

### Scope

Portable/full-driver distribution split.

### Focus

```text
portable package remains driver-independent
full package includes validated driver artifacts
installer handles optional driver correctly
checksums/manifests generated
release workflow keeps privileged driver path separate
```

### Preserve

Portable qpwgraph MUST continue to work without the optional Windows audio driver.

### Avoid

Do not modify driver implementation.

Do not modify application-routing policy.

---

# PHASE 1 MERGE CHECKPOINT

The coordinator integrates Phase 1 branches one at a time.

Required after every merge:

```text
cargo/build checks
relevant Windows unit tests
driver package audit when driver files changed
portable-mode smoke check
git diff inspection
```

Resolve integration conflicts here.

Do NOT ask feature agents to independently merge each other's branches.

Create one shared integration commit:

```text
PARITY_RC_1=<commit>
```

All Phase 2 agents validate exactly this commit.

---

# PHASE 2 — PARALLEL VALIDATION ON ONE RELEASE CANDIDATE

No feature development should happen in this phase unless validation discovers a defect.

All validators use exactly `PARITY_RC_1`.

---

## Validator 2A — Application Routing

Validate:

```text
automatic route apply
all required Windows audio roles
application restart
replacement PID
rule removal
backend shutdown
exact restoration
manual user override
unsupported-build fallback
destination loss
```

Multi-Windows-build results must be recorded separately.

---

## Validator 2B — Driver Stress / Lifecycle

Validate:

```text
ordinary stress matrix
two-cable isolation
EOS
timing
stream lifecycle
independent-client crash
backend crash/recovery
sleep/resume
device disable/enable
AudioSrv restart
reboot/lifecycle where available
```

Preserve failures as failures.

Never overwrite failed evidence with a later passing run.

---

## Validator 2C — Driver Verifier

Run the approved Verifier configuration against `PARITY_RC_1`.

Collect:

```text
Verifier configuration
stress result
event/error evidence
crash dumps if any
candidate hashes
```

A normal stress run without Verifier does not satisfy this gate.

---

## Validator 2D — HLK

On the appropriate HLK environment:

```text
prepare exact candidate
run relevant required tests
export results
record package/driver hash
archive evidence
```

Repository tooling preparation alone does not close this gate.

---

## Validator 2E — Secure Boot

Use a separately configured test environment.

Do NOT alter the development PC's Test Mode configuration.

Validate the exact release candidate under the required Secure Boot configuration.

---

## Validator 2F — Installer / Portable Regression

Validate both distributions independently.

### Portable

```text
no driver required
application starts
existing Core Audio features work
MIDI works
process loopback works where supported
```

### Full

```text
installer succeeds
driver package identity correct
four provider roles enumerate
uninstall/upgrade behavior correct
```

---

# PHASE 2 RESULT

Each validator returns only:

```text
STATUS: PASS | FAIL | BLOCKED

COMMIT:
WINDOWS_BUILD:
DRIVER_VERSION:
PACKAGE_HASH:

TESTS_RUN:
...

EVIDENCE:
...

FAILURES:
...

CODE_CHANGES_REQUIRED:
yes/no
```

Validators should not quietly fix unrelated failures.

Failures go back to the matching Phase 1 owner.

---

# PHASE 3 — FIX ROUND

Run only for failed Phase 2 gates.

Create one agent per failure domain.

Example:

```text
Verifier defect          -> Driver agent
App policy defect        -> Routing agent
Backend recovery defect  -> Recovery agent
Installer defect         -> Packaging agent
```

Rules:

1. Start from `PARITY_RC_1`.
2. Make the smallest possible fix.
3. Add a regression test when possible.
4. Do not refactor unrelated code.
5. Return one narrow PR/commit.
6. Rerun only affected validation first.
7. After fixes merge, create:

```text
PARITY_RC_2=<commit>
```

Then rerun all release-critical Phase 2 validators against RC2.

---

# PHASE 4 — EXTERNAL RELEASE GATES

These gates are intentionally sequential where artifact identity matters.

Use one immutable candidate.

```text
locked source commit
    ↓
release driver package
    ↓
Verifier PASS
    ↓
required lifecycle PASS
    ↓
HLK PASS
    ↓
Secure Boot PASS
    ↓
Microsoft submission
    ↓
returned Microsoft-signed package verification
    ↓
full installer using exact returned package
```

Do not rebuild the driver between gates unless a failure requires a new candidate.

If the binary changes, previous binary-specific evidence must not automatically be transferred to the new candidate.

---

# PHASE 5 — DOCUMENTATION + FINAL PARITY AUDIT

Run after implementation and validation evidence stabilizes.

## Agent 5A — Documentation Auditor

This agent should make documentation changes only.

Re-read current source and evidence.

For every Windows feature classify it as:

```text
SUPPORTED
EXPERIMENTAL
PORTABLE_ONLY
DRIVER_REQUIRED
VALIDATED
EXTERNAL_GATE_OPEN
UNSUPPORTED
```

Remove stale roadmap claims.

Do not mark functionality complete from old plan text.

---

## Agent 5B — Final Acceptance Auditor

Prefer an agent that did not implement the features.

Check:

```text
portable mode works without driver
kernel remains minimal transport/provider
no project-authored C/C++ runtime returned
automatic routing fails closed
no guessed private COM ABI behavior
Verifier evidence corresponds to candidate
HLK evidence corresponds to candidate
Secure Boot evidence corresponds to candidate
Microsoft signature corresponds to submitted candidate
installer embeds exact accepted driver
documentation matches source/evidence
```

Produce the final parity report.

---

# AGENT WORK CONTRACT

Send this block with every task given to an LLM agent:

```text
Repository: nglmercer/qpwgraph-rs
Base commit: <SHA>
Your branch/worktree: <name>
Your phase: <phase>
Your ownership: <files/directories>

GOAL
<one narrow outcome>

READ FIRST
<relevant files>

ALLOWED TO EDIT
<exact paths>

DO NOT EDIT
<paths owned by other agents>

DEPENDENCIES
<interfaces/commits>

ACCEPTANCE CRITERIA
<observable requirements>

TESTS
<commands>

EVIDENCE REQUIRED
<logs/json/hash/etc>

HANDOFF
Return:
1. summary
2. files changed
3. tests run + results
4. evidence generated
5. remaining blockers
6. commit SHA
7. anything another agent must know
```

---

# RULES FOR PARALLEL AGENTS

1. One worktree/branch per agent.
2. Start every wave from the same base commit.
3. Assign file ownership before implementation.
4. No agent edits another agent's owned files.
5. Shared interfaces belong to the coordinator.
6. Prefer adding narrow modules/tests rather than broad refactors.
7. Agents commit their work; they do not merge siblings.
8. One integration agent performs merges.
9. Validation agents test an immutable candidate SHA.
10. A failed live run remains recorded as a failure.
11. Compile success is not live Windows validation.
12. Never rebuild between release gates without issuing a new candidate identity.
13. Current source and retained evidence override stale roadmap text.

---

# FASTEST EXECUTION SHAPE

```text
                  PHASE 0
               Baseline Agent
                     |
        +------------+-------------+
        |            |             |
        v            v             v
     Agent 1A     Agent 1B      Agent 1C
     App policy   Driver        Recovery
        |          lifecycle       |
        +-----+------+-------------+
              |
        Agent 1D     Agent 1E     Agent 1F
        Verifier     Release      Packaging
           \            |            /
            \           |           /
             +---- INTEGRATOR -----+
                       |
                  PARITY_RC_1
                       |
       +---------------+----------------+
       |       |       |       |        |
      2A      2B      2C      2D       2E/2F
     route   driver  verifier  HLK     SB/package
       |       |       |       |        |
       +-------+-------+-------+--------+
                       |
                 PASS / FIX ROUND
                       |
                 immutable candidate
                       |
               external release gates
                       |
                 docs + final audit
```

The key change is that **implementation is parallel, validation is parallel, but release-candidate identity and final external gates are synchronized**.
