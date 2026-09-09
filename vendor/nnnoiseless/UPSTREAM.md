# Vendored provenance

This directory is synchronized from `https://github.com/nglmercer/nnnoiseless`
at commit `2a1ea16e1429d71b553f0b084cd6e14694d197d1` (Hush backend merge).

The Hush adapter expects the Hush model bundle from the `weya-ai/hush`
repository at revision `e4ce7721d519438e8c2fa0b1f7cc27e1868fff99`:
`advanced_dfnet16k_model_best_onnx.tar.gz`.

Pinned SHA-256:

```text
45632ccaa82b71bb743d6caa7c78e983fe2f2790a3af7f6ec48e6ed7ba085df6
```

The release copy is embedded at
`crates/pw-graph-effects/resources/hush/advanced_dfnet16k_model_best_onnx.tar.gz`
and is accompanied by its Apache-2.0 model notice. An environment override is
available for development and CI, but production startup never downloads a
model.

Its DeepFilterNet runtime is pinned to the `v0.5.3` commit
`10d947c2b183934f34a8852a082e8a0f3c53fbb1`.

The vendor keeps the upstream Hush implementation and tests, while its
feature table is reduced to qpwgraph's library use: the CLI, microphone
example, DASP adapters, and browser bindings are not enabled. qpwgraph adds
the `hush` feature to `pw-graph-effects`; production model loading is handled
by that crate on a control/worker thread and never by `process()`.

The September 2026 qpwgraph adapter fix changes no vendor DSP or neural weights.
It replaces callback-sequence scheduling with host-frame timelines, preserves
native frame assembly/resampler continuity, and synchronously acknowledges
worker initialization. The worker uses a bounded useful backlog and a separate
wet epoch for hard overload resynchronization, so stale input is discarded and
the aligned dry timeline remains continuous while Hush warms again. The
resampler's centered sinc lookahead delays sample availability without shifting
sample positions. Direct model tests and a separate qpwgraph-to-direct-Hush
reference test cover the two layers independently.
