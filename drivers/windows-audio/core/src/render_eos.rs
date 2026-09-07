//! Rust-owned render end-of-stream packet boundary policy.
//!
//! ACX still owns packet delivery, but the decision about how many bytes of a
//! render packet may enter a virtual cable is independent of WDK types. Keeping
//! it here makes the safety rule testable on every host and leaves only the
//! ABI-shaped transport call at the ACX runtime boundary.

/// The audio prefix that may be copied from one render packet.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderPayload {
    pub bytes: u32,
    pub eos_state: i32,
}

/// Apply the ACX render EOS boundary.
///
/// eos_state is the ACX runtime's state machine: 0 means no EOS has been observed,
/// 1 means the final packet and byte prefix have been published, and 2 means
/// the final packet has already been consumed. Packet sequence comparisons
/// intentionally use signed 32-bit distance, matching the Windows
/// ULONG/LONG rollover rule.
pub const fn render_payload(
    packet: u32,
    packet_bytes: u32,
    eos_state: i32,
    eos_packet: u32,
    eos_bytes: u32,
) -> RenderPayload {
    if !matches!(eos_state, 0..=2) {
        return RenderPayload {
            bytes: 0,
            eos_state: 2,
        };
    }
    let mut result = RenderPayload {
        bytes: packet_bytes,
        eos_state,
    };
    if eos_state == 2 {
        result.bytes = 0;
    } else if eos_state == 1 && packet.wrapping_sub(eos_packet) as i32 >= 0 {
        result.bytes = if packet == eos_packet && eos_bytes <= packet_bytes {
            eos_bytes
        } else {
            0
        };
        result.eos_state = 2;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{render_payload, RenderPayload};

    #[test]
    fn ordinary_and_preceding_packets_keep_their_full_payload() {
        assert_eq!(
            render_payload(9, 1920, 0, 0, 0),
            RenderPayload {
                bytes: 1920,
                eos_state: 0
            }
        );
        assert_eq!(
            render_payload(9, 1920, 1, 10, 40),
            RenderPayload {
                bytes: 1920,
                eos_state: 1
            }
        );
    }

    #[test]
    fn final_packet_keeps_only_its_declared_prefix() {
        for bytes in (0..=1920).step_by(4) {
            let payload = render_payload(10, 1920, 1, 10, bytes);
            assert_eq!(
                payload,
                RenderPayload {
                    bytes,
                    eos_state: 2
                }
            );
            assert_eq!(
                render_payload(11, 1920, payload.eos_state, 10, bytes).bytes,
                0
            );
        }
    }

    #[test]
    fn skipped_or_malformed_eos_is_silence_after_the_boundary() {
        assert_eq!(
            render_payload(11, 1920, 1, 10, 40),
            RenderPayload {
                bytes: 0,
                eos_state: 2
            }
        );
        assert_eq!(
            render_payload(10, 1920, 1, 10, 1924),
            RenderPayload {
                bytes: 0,
                eos_state: 2
            }
        );
    }

    #[test]
    fn sequence_rollover_does_not_replay_audio() {
        assert_eq!(render_payload(u32::MAX, 1920, 1, 0, 40).bytes, 1920);
        assert_eq!(
            render_payload(0, 1920, 1, u32::MAX, 40),
            RenderPayload {
                bytes: 0,
                eos_state: 2
            }
        );
    }

    #[test]
    fn consumed_eos_state_suppresses_every_later_packet() {
        assert_eq!(render_payload(3, 1920, 2, 3, 40).bytes, 0);
    }

    #[test]
    fn unknown_eos_state_fails_closed() {
        assert_eq!(
            render_payload(3, 1920, 99, 3, 40),
            RenderPayload {
                bytes: 0,
                eos_state: 2
            }
        );
    }
}
