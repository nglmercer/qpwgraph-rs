# Windows application routing

There is no documented public Core Audio operation that moves another
application's session to a different endpoint. qpwgraph therefore separates
the supported manual path from the experimental policy boundary in
`windows::app_route_policy`.

The default `UnsupportedAppRoutePolicy` never calls an undocumented interface.
It returns an actionable instruction:

> Set this app's output to QPWGraph Virtual Output in Settings > System > Sound
> > Volume mixer.

Once the session is observed on that virtual render endpoint, qpwgraph can
capture its process tree, route the PCM through the user-mode router, insert
effects, meter true RMS, and render the processed result to a chosen physical
destination. If qpwgraph cannot prove the original stream is isolated, route
creation is refused so dry and processed audio are never silently doubled.

Persisted application routes use an executable/package selector, destination
stable endpoint selector, virtualization-required flag, effect-chain IDs,
gain, and enabled state. They never persist only a PID. The configuration
layer provides stable-selector matching (with the most-specific rule winning);
automatic route restoration still requires a unique live identity and the
provider-verified virtual endpoint, and must not be inferred from a matching
display name.

`WindowsConfig.experimental_app_routing` is false by default. The only policy
boundary is `windows::audio_policy_config::VerifiedAudioPolicyConfig`. It owns
an explicit Windows 10 `AudioPolicyConfig` vtable declaration, the downlevel
IID, and a narrow build table covering 19041 through 19045. Construction
activates that exact interface before reporting `Experimental`; unsupported
builds, activation failures, and any HRESULT failure remain `ManualOnly` or
degraded with diagnostics. The Windows 11 IID is recorded but is not enabled
until the same method layout has live evidence on Windows 11.

Automatic isolation revalidates the live process identity before every private
policy call, reads all three render roles, saves the prior endpoint in a
runtime-only lease, and sets the provider-verified QPWGraph Virtual Output.
The lease also remembers the runtime PID that received the write; this PID is
never persisted, and a selector-matched process restart reuses the saved
original endpoints while applying the virtual target to the replacement PID.
The normal reconciler still waits for a Core Audio snapshot to prove the
session moved before opening process loopback or creating effects. Restore
uses only the endpoint qpwgraph applied; a user change marks the lease
non-owned and is preserved. The global clear-all method is never used.
Driver shutdown also attempts the same ownership-checked restore before the
worker exits, so an ordinary qpwgraph restart does not abandon a live lease.

The release and support report records policy mode, OS build, selected
interface version, last operation/HRESULT, and fallback reason. It never
records a process path, raw endpoint property blob, or PID as persisted
identity. Automatic switching is therefore an explicit experimental boundary,
not a hidden fallback around the manual Volume Mixer workflow.

The opt-in live probe
`PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE=1 cargo test -p windows-audio-test-tone
--features relay-tests --test relay_microphone --
experimental_application_route_rebinds_default_helper_and_restores` starts the
deterministic helper on its ordinary default endpoint, verifies all three
persisted render roles move to `app-render`, restarts the helper to confirm the
new session is isolated before qpwgraph activates process capture, and removes
the rule to verify the original role-specific endpoints are restored. It
requires the existing signed virtual-audio package and a physical render
endpoint; it is never run by default.

The shutdown-specific probe uses the same setup with
`PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE_DROP=1` and the test name
`experimental_application_route_restores_on_driver_shutdown`. It leaves the
replacement helper and active lease in place, drops the driver, and then reads
the three role values directly to verify destructor-time restoration. It is
also opt-in and should be run only on a disposable test profile.
