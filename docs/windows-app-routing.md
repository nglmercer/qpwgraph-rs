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
full automatic route restoration is still gated on the driver/app-session
activation work and must not be inferred from a matching display name.

`WindowsConfig.experimental_app_routing` is false by default. An eventual
WinRT `AudioPolicyConfig` implementation must use explicit, verified interface
declarations, query only known IIDs, gate every supported Windows build family,
and remain optional. Unknown vtable probing and startup dependency on that ABI
are prohibited.
