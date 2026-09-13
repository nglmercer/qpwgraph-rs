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

/// The result of checking the next packet number supplied by an ACX render
/// client. ACX exposes a ULONG packet sequence, so the public number wraps at
/// `u32::MAX`; the exact next value is therefore compared with wrapping
/// arithmetic rather than ordinary unsigned ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderPacketOrder {
    Next,
    Late,
    Skipped,
}

/// The only render packet flag currently consumed by this driver.
pub const RENDER_EOS_FLAG: u32 = 0x0000_0200;

/// Validate the flag/length pair passed to the ACX render callback.
///
/// ACX only interprets `eos_packet_length` when EOS is set. A non-EOS packet
/// may carry any length value because that field is ignored by the contract;
/// the transport callback still bounds the actual copied packet separately.
pub const fn render_packet_arguments_valid(
    flags: u32,
    eos_packet_length: u32,
    packet_bytes: u32,
) -> bool {
    if flags & !RENDER_EOS_FLAG != 0 {
        return false;
    }
    flags & RENDER_EOS_FLAG == 0
        || (eos_packet_length <= packet_bytes && eos_packet_length.is_multiple_of(4))
}

/// Classify a render packet against the last completed packet.
///
/// Only the exact successor is accepted. For any other value, signed
/// 32-bit distance preserves the usual half-range sequence-number rule when
/// deciding whether the packet is late or skips ahead. The exact-successor
/// check is performed first so `u32::MAX -> 0` is unambiguous.
pub const fn classify_render_packet(current: u32, packet: u32) -> RenderPacketOrder {
    if packet == current.wrapping_add(1) {
        RenderPacketOrder::Next
    } else if packet.wrapping_sub(current) as i32 <= 0 {
        RenderPacketOrder::Late
    } else {
        RenderPacketOrder::Skipped
    }
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
    use super::{
        classify_render_packet, render_packet_arguments_valid, render_payload, RenderPacketOrder,
        RenderPayload, RENDER_EOS_FLAG,
    };

    #[test]
    fn non_eos_length_is_ignored_but_eos_length_is_bounded() {
        assert!(render_packet_arguments_valid(0, u32::MAX, 1920));
        assert!(render_packet_arguments_valid(RENDER_EOS_FLAG, 0, 1920));
        assert!(render_packet_arguments_valid(RENDER_EOS_FLAG, 1920, 1920));
        assert!(!render_packet_arguments_valid(RENDER_EOS_FLAG, 1924, 1920));
        assert!(!render_packet_arguments_valid(RENDER_EOS_FLAG, 3, 1920));
        assert!(!render_packet_arguments_valid(0x400, 0, 1920));
    }

    #[test]
    fn render_packet_sequence_accepts_wrap_and_rejects_skip_or_late() {
        assert_eq!(classify_render_packet(41, 42), RenderPacketOrder::Next);
        assert_eq!(classify_render_packet(41, 41), RenderPacketOrder::Late);
        assert_eq!(classify_render_packet(41, 43), RenderPacketOrder::Skipped);
        assert_eq!(classify_render_packet(u32::MAX, 0), RenderPacketOrder::Next);
        assert_eq!(
            classify_render_packet(u32::MAX, 1),
            RenderPacketOrder::Skipped
        );
        assert_eq!(classify_render_packet(0, u32::MAX), RenderPacketOrder::Late);
    }

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
