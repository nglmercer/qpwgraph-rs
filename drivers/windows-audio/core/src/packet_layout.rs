//! ACX PCM16 stereo packet geometry, independent of WDK allocation APIs.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketLayout {
    pub allocation_bytes: u32,
    pub packet_bytes: u32,
    pub first_offset: u32,
}

/// A single packet is mapped twice, so its audio extent must cover whole
/// pages starting at offset zero. Two independent packet buffers instead
/// place the first packet at the end of its allocation and the second at
/// offset zero, allowing consecutive user-mode mappings without a gap.
pub fn pcm16_packet_layout(
    count: u32,
    requested_bytes: u32,
    page_bytes: u32,
    max_payload_bytes: u32,
) -> Option<PacketLayout> {
    if !matches!(count, 1 | 2)
        || requested_bytes == 0
        || !requested_bytes.is_multiple_of(4)
        || requested_bytes > max_payload_bytes
        || page_bytes < 4
        || !page_bytes.is_power_of_two()
    {
        return None;
    }
    let allocation_bytes = requested_bytes.checked_add(page_bytes - 1)? & !(page_bytes - 1);
    let packet_bytes = if count == 1 {
        allocation_bytes
    } else {
        requested_bytes
    };
    // Rounding a timer-driven packet must not exceed the transport's bounded
    // callback length, even when the original request fit inside the limit.
    if packet_bytes > max_payload_bytes {
        return None;
    }
    Some(PacketLayout {
        allocation_bytes,
        packet_bytes,
        first_offset: allocation_bytes - packet_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_ms_single_packet_covers_the_entire_double_mapping() {
        assert_eq!(
            pcm16_packet_layout(1, 1920, 4096, 192000),
            Some(PacketLayout {
                allocation_bytes: 4096,
                packet_bytes: 4096,
                first_offset: 0,
            })
        );
        assert_eq!(
            pcm16_packet_layout(2, 1920, 4096, 192000),
            Some(PacketLayout {
                allocation_bytes: 4096,
                packet_bytes: 1920,
                first_offset: 2176,
            })
        );
    }

    #[test]
    fn every_supported_size_obeys_mapping_and_transport_bounds() {
        for count in [1, 2] {
            for requested in (4..=192000).step_by(4) {
                let Some(layout) = pcm16_packet_layout(count, requested, 4096, 192000) else {
                    assert_eq!(count, 1);
                    assert!(requested > 188416);
                    continue;
                };
                assert_eq!(layout.allocation_bytes % 4096, 0);
                assert_eq!(
                    layout.first_offset + layout.packet_bytes,
                    layout.allocation_bytes
                );
                assert!(layout.packet_bytes >= requested && layout.packet_bytes <= 192000);
                if count == 1 {
                    assert_eq!(layout.first_offset, 0);
                    assert_eq!(layout.packet_bytes % 4096, 0);
                } else {
                    assert_eq!(layout.packet_bytes, requested);
                }
            }
        }
    }

    #[test]
    fn malformed_and_overflowing_requests_are_rejected() {
        for args in [
            (0, 1920, 4096, 192000),
            (3, 1920, 4096, 192000),
            (1, 0, 4096, 192000),
            (1, 1919, 4096, 192000),
            (1, 192004, 4096, 192000),
            (1, 192000, 4096, 192000),
            (1, 1920, 0, 192000),
            (1, 1920, 3, 192000),
            (1, 1920, 4095, 192000),
            (2, u32::MAX - 3, 4096, u32::MAX),
        ] {
            assert_eq!(pcm16_packet_layout(args.0, args.1, args.2, args.3), None);
        }
    }
}
