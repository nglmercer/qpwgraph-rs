//! Every bound the engine enforces, in one place.
//!
//! These are not tuning knobs: each one caps something an untrusted peer can
//! make this process allocate or retry, so a reader auditing the engine's
//! resource exposure should need to look only here.

use super::*;

/// The largest buffer a realtime audio callback may hand to
/// [`RelayHandle::try_pull_playback`] or [`RelayHandle::try_push_capture`],
/// in samples.
///
/// Everything the realtime path might otherwise have to grow — the mixing
/// scratch, each session's conversion buffers — is sized from this at setup
/// time. A callback presenting more than this gets served only this much
/// rather than triggering an allocation on the audio thread.
pub const MAX_REALTIME_QUANTUM_SAMPLES: usize = 16_384;

/// Upper bound on queued events.
///
/// The queue used to be unbounded, which made it a memory-growth path: a peer
/// sending malformed audio produced one error event per datagram, and a UI
/// that drains once per frame could never keep up with a flood. Dropping the
/// oldest events is right — a consumer that has fallen this far behind wants
/// the recent state of the world, not a backlog.
pub const MAX_QUEUED_EVENTS: usize = 256;

/// Maximum number of host-side enrollment transactions waiting for an
/// embedding to durably commit them. Keeping this bounded prevents a peer
/// from turning the application callback into an allocation DoS.
pub const MAX_PENDING_TRUST_ENROLLMENTS: usize = 64;
/// A host embedding has this long to persist an enrollment and accept it.
///
/// This is the window a UI has to show its accept/decline decision, so it must
/// be comfortable for a human. The paired client keeps waiting (and
/// keepalive-ing) for the acknowledgement for this long plus handshake slack —
/// a decision that arrives within the window must never reach a client that has
/// already given up, or the host stores a credential the client discarded and
/// every later reconnect falls back to PIN pairing.
pub const TRUST_ENROLLMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum discovered addresses retained for one stable peer identity.
pub const MAX_TRUSTED_CANDIDATE_ADDRESSES: usize = 16;
/// Maximum `(peer, address)` failure records retained for candidate backoff.
pub const MAX_TRUSTED_CANDIDATE_FAILURES: usize = 1024;
/// Maximum discovered addresses retained across all stable peer identities.
/// Discovery is untrusted input and must not be allowed to grow metadata maps.
pub const MAX_DISCOVERED_PEER_ADDRESSES: usize = 4096;
/// Maximum stable identities retained in the last-success preference cache.
pub const MAX_TRUSTED_SUCCESSFUL_ADDRESSES: usize = 1024;
/// Maximum trusted credentials accepted from an embedding or persistence
/// layer. Trusted-device management is user-controlled, but its backing table
/// still needs a hard bound against malformed or stale configuration.
pub const MAX_TRUSTED_PEERS: usize = 256;

/// Failed pairings one source address may make before it is locked out.
pub const PAIRING_ATTEMPT_LIMIT: u32 = 5;
/// How long a source stays locked out. With a PAKE, guessing a six-digit PIN
/// is an online-only game; at five tries per lockout it would take centuries.
pub const PAIRING_LOCKOUT: Duration = Duration::from_secs(60);
/// Maximum number of source addresses retained in the pairing rate limiter.
///
/// This is deliberately a hard cap, rather than a cleanup threshold: a flood
/// of distinct source addresses must not turn the limiter itself into an
/// unbounded allocation.
pub(crate) const MAX_PAIRING_FAILURE_RECORDS: usize = 1024;
