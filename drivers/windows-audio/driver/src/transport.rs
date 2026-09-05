//! Rust-owned PCM transport used by the opt-in ACX adapter.
//!
//! The ACX callback and WDF timer stay in the small C ABI bridge because the
//! WDK headers contain versioned macros and opaque object layouts.  The audio
//! data itself crosses into this module through fixed, allocation-free FFI
//! helpers, so the bounded SPSC policy in `qpwgraph-audio-core` is the actual
//! cable implementation rather than a C-side placeholder.

use crate::ring::{PushResult, SpscSampleRing};

const CABLE_SAMPLES: usize = 48_000 * 2;

static APP_CABLE: SpscSampleRing<CABLE_SAMPLES> = SpscSampleRing::new();
static RELAY_CABLE: SpscSampleRing<CABLE_SAMPLES> = SpscSampleRing::new();

fn pcm16_to_f32(low: u8, high: u8) -> f32 {
    f32::from(i16::from_le_bytes([low, high])) / 32_768.0
}

fn f32_to_pcm16(sample: f32) -> i16 {
    let bounded = if sample.is_nan() {
        0.0
    } else {
        sample
    };
    if bounded <= -1.0 {
        i16::MIN
    } else if bounded >= 1.0 {
        i16::MAX
    } else {
        (bounded * 32_768.0)
            .round()
            .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
    }
}

/// Push a PCM16 packet into the bounded render-to-monitor cable.
///
/// The return value is the number of input bytes rejected because the ring
/// was full.  The ACX stream path does not allocate, block, or log here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpwgraph_audio_transport_push_pcm16(data: *const u8, bytes: u32) -> u32 {
    unsafe { push_pcm16_into(&APP_CABLE, data, bytes) }
}

/// Push a PCM16 packet into the independent relay sink-to-microphone cable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpwgraph_audio_transport_push_relay_pcm16(
    data: *const u8,
    bytes: u32,
) -> u32 {
    unsafe { push_pcm16_into(&RELAY_CABLE, data, bytes) }
}

unsafe fn push_pcm16_into(
    cable: &SpscSampleRing<CABLE_SAMPLES>,
    data: *const u8,
    bytes: u32,
) -> u32 {
    if data.is_null() || bytes < 2 {
        return 0;
    }

    let sample_bytes = bytes & !1;
    let mut dropped = 0u32;
    for offset in (0..sample_bytes).step_by(2) {
        // SAFETY: the ACX packet allocator supplies a valid buffer for the
        // exact packet length passed by the bridge.
        let sample =
            unsafe { pcm16_to_f32(*data.add(offset as usize), *data.add(offset as usize + 1)) };
        if let PushResult::DroppedNewest { .. } = cable.push(&[sample]) {
            dropped = dropped.saturating_add(2);
        }
    }
    dropped
}

/// Pop a PCM16 packet from the cable, filling underflow with explicit zeroes.
///
/// The return value is always the number of complete bytes written.  The
/// core ring records underflow/discontinuity counters for diagnostic use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpwgraph_audio_transport_pop_pcm16(data: *mut u8, bytes: u32) -> u32 {
    unsafe { pop_pcm16_from(&APP_CABLE, data, bytes) }
}

/// Pop a PCM16 packet from the independent relay sink-to-microphone cable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpwgraph_audio_transport_pop_relay_pcm16(
    data: *mut u8,
    bytes: u32,
) -> u32 {
    unsafe { pop_pcm16_from(&RELAY_CABLE, data, bytes) }
}

unsafe fn pop_pcm16_from(
    cable: &SpscSampleRing<CABLE_SAMPLES>,
    data: *mut u8,
    bytes: u32,
) -> u32 {
    if data.is_null() || bytes < 2 {
        return 0;
    }

    let sample_bytes = bytes & !1;
    for offset in (0..sample_bytes).step_by(2) {
        let mut sample = [0.0f32; 1];
        cable.pop_or_silence(&mut sample);
        let encoded = f32_to_pcm16(sample[0]).to_le_bytes();
        // SAFETY: the ACX packet allocator supplies a writable buffer for the
        // exact packet length passed by the bridge.
        unsafe {
            *data.add(offset as usize) = encoded[0];
            *data.add(offset as usize + 1) = encoded[1];
        }
    }
    sample_bytes
}

/// Clear queued audio after both ACX streams have stopped or the device is
/// being released.  The bridge only calls this when no producer or consumer
/// callback can still be using the ring.
#[unsafe(no_mangle)]
pub extern "C" fn qpwgraph_audio_transport_clear() {
    APP_CABLE.clear();
}

/// Clear the relay cable after both relay streams stop or the device is
/// released. This is separate from the app cable so one virtual endpoint pair
/// can never leak audio into the other pair during restart.
#[unsafe(no_mangle)]
pub extern "C" fn qpwgraph_audio_transport_clear_relay() {
    RELAY_CABLE.clear();
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, vec::Vec};

    use super::*;

    static CABLE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn pcm16_bytes(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    #[test]
    fn pcm16_transport_preserves_all_representable_samples() {
        let _guard = CABLE_TEST_LOCK.lock().unwrap();
        qpwgraph_audio_transport_clear();
        let input = [-32_768, -32_767, -1, 0, 1, 32_766, 32_767];
        let input_bytes = pcm16_bytes(&input);
        assert_eq!(
            unsafe {
                qpwgraph_audio_transport_push_pcm16(input_bytes.as_ptr(), input_bytes.len() as u32)
            },
            0
        );

        let mut output = vec![0_u8; input_bytes.len()];
        assert_eq!(
            unsafe {
                qpwgraph_audio_transport_pop_pcm16(
                    output.as_mut_ptr(),
                    output.len() as u32,
                )
            },
            input_bytes.len() as u32
        );
        assert_eq!(output, input_bytes);
    }

    #[test]
    fn app_and_relay_cables_are_independent() {
        let _guard = CABLE_TEST_LOCK.lock().unwrap();
        qpwgraph_audio_transport_clear();
        qpwgraph_audio_transport_clear_relay();
        let input = pcm16_bytes(&[1_024, -2_048]);
        assert_eq!(
            unsafe {
                qpwgraph_audio_transport_push_pcm16(input.as_ptr(), input.len() as u32)
            },
            0
        );

        let mut relay_output = vec![0x7f_u8; input.len()];
        assert_eq!(
            unsafe {
                qpwgraph_audio_transport_pop_relay_pcm16(
                    relay_output.as_mut_ptr(),
                    relay_output.len() as u32,
                )
            },
            input.len() as u32
        );
        assert_eq!(relay_output, vec![0; input.len()]);

        let mut app_output = vec![0_u8; input.len()];
        unsafe {
            qpwgraph_audio_transport_pop_pcm16(
                app_output.as_mut_ptr(),
                app_output.len() as u32,
            );
        }
        assert_eq!(app_output, input);
    }

    #[test]
    fn underflow_is_explicit_silence() {
        let _guard = CABLE_TEST_LOCK.lock().unwrap();
        qpwgraph_audio_transport_clear();
        let mut output = vec![0xa5_u8; 12];
        unsafe {
            qpwgraph_audio_transport_pop_pcm16(output.as_mut_ptr(), output.len() as u32);
        }
        assert_eq!(output, vec![0; 12]);
    }
}
