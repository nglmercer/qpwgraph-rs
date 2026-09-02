//! Audio codecs for the relay audio channel.
//!
//! Two implementations share the [`AudioEncode`]/[`AudioDecode`] traits:
//! Opus (default, small packets) and raw f32 PCM (debugging and
//! deterministic tests). Decoders must support packet-loss concealment via
//! [`AudioDecode::conceal`] because the UDP channel drops packets.

use crate::protocol::CodecKind;
use crate::RelayError;
use std::fmt;

/// Per-channel Opus bitrate. Music through the device-playback capture is the
/// demanding case on this path; 96 kbps per channel sits well inside Opus's
/// near-transparent range while the largest supported packet — stereo 60 ms at
/// constrained VBR — still stays an order of magnitude below one datagram.
const OPUS_BITRATE_PER_CHANNEL: i32 = 96_000;
/// Encoder complexity (0–10). High, because the quality gap shows up exactly
/// where this relay is used — full-band music playback capture — while the
/// absolute cost stays small: one 20 ms frame encodes in about a millisecond
/// on any phone made this decade, and the capture queue absorbs the jitter.
const OPUS_COMPLEXITY: i32 = 8;

/// Encode one frame of interleaved f32 PCM into `out`.
pub trait AudioEncode: Send {
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<usize, RelayError>;
}

/// Decode one received frame into interleaved f32 PCM.
pub trait AudioDecode: Send {
    /// Decode `payload`, returning the number of samples written to `out`.
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> Result<usize, RelayError>;

    /// Synthesize concealment for one lost frame, returning the number of
    /// samples written to `out`.
    fn conceal(&mut self, out: &mut [f32]) -> Result<usize, RelayError>;
}

/// Frame geometry shared by both ends of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_ms: u16,
}

impl AudioFormat {
    pub fn new(sample_rate: u32, channels: u16, frame_ms: u16) -> Self {
        Self {
            sample_rate,
            channels,
            frame_ms,
        }
    }

    /// Interleaved samples per frame (`frame_ms` worth of audio).
    pub fn frame_samples(&self) -> usize {
        (self.sample_rate as usize / 1000) * self.frame_ms as usize * self.channels as usize
    }

    pub fn is_stereo(&self) -> bool {
        self.channels >= 2
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Hz, {} ch, {} ms frames",
            self.sample_rate, self.channels, self.frame_ms
        )
    }
}

pub fn make_encoder(
    codec: CodecKind,
    format: AudioFormat,
) -> Result<Box<dyn AudioEncode>, RelayError> {
    match codec {
        CodecKind::Pcm => Ok(Box::new(PcmEncoder)),
        CodecKind::Opus => Ok(Box::new(OpusEncoderState::new(format)?)),
    }
}

pub fn make_decoder(
    codec: CodecKind,
    format: AudioFormat,
) -> Result<Box<dyn AudioDecode>, RelayError> {
    match codec {
        CodecKind::Pcm => Ok(Box::new(PcmDecoder { format })),
        CodecKind::Opus => Ok(Box::new(OpusDecoderState::new(format)?)),
    }
}

/// Raw little-endian f32 PCM, mostly for deterministic tests and debugging.
struct PcmEncoder;

impl AudioEncode for PcmEncoder {
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<usize, RelayError> {
        out.clear();
        out.reserve(pcm.len() * 4);
        for sample in pcm {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(out.len())
    }
}

struct PcmDecoder {
    format: AudioFormat,
}

impl AudioDecode for PcmDecoder {
    /// Decode exactly one negotiated frame.
    ///
    /// The length check is strict on purpose. A framed realtime protocol has
    /// exactly one right size for a packet; silently ignoring trailing bytes
    /// or accepting a short frame lets a malformed or truncated datagram
    /// perturb the stream's timing instead of being dropped as the corrupt
    /// packet it is.
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> Result<usize, RelayError> {
        let expected = self.format.frame_samples();
        if payload.len() != expected * 4 {
            return Err(RelayError::Codec(format!(
                "PCM frame of {} bytes is not the negotiated {} bytes",
                payload.len(),
                expected * 4
            )));
        }
        let samples = expected;
        if samples > out.len() {
            return Err(RelayError::Codec(format!(
                "PCM frame of {samples} samples exceeds the {} sample buffer",
                out.len()
            )));
        }
        for (index, slot) in out.iter_mut().take(samples).enumerate() {
            let bytes = payload[index * 4..index * 4 + 4]
                .try_into()
                .expect("slice is exactly 4 bytes");
            *slot = f32::from_le_bytes(bytes);
        }
        Ok(samples)
    }

    fn conceal(&mut self, out: &mut [f32]) -> Result<usize, RelayError> {
        let samples = self.format.frame_samples().min(out.len());
        out[..samples].fill(0.0);
        Ok(samples)
    }
}

struct OpusEncoderState {
    encoder: opus::Encoder,
}

impl OpusEncoderState {
    fn new(format: AudioFormat) -> Result<Self, RelayError> {
        let channels = opus_channels(format.channels)?;
        // `LowDelay` is Opus's RESTRICTED_LOWDELAY mode: CELT only, with no
        // SILK layer and therefore none of its 5 ms encoder lookahead. That
        // costs inband FEC — a SILK feature — but the receiver already
        // conceals losses through the jitter buffer, and the lookahead is
        // pure added delay on every single frame.
        let mut encoder =
            opus::Encoder::new(format.sample_rate, channels, opus::Application::LowDelay)
                .map_err(|error| RelayError::Codec(format!("Opus encoder init: {error}")))?;
        let setting = |result: Result<(), opus::Error>, what: &str| {
            result.map_err(|error| RelayError::Codec(format!("Opus {what}: {error}")))
        };
        setting(
            encoder.set_bitrate(opus::Bitrate::Bits(
                OPUS_BITRATE_PER_CHANNEL * i32::from(format.channels),
            )),
            "bitrate",
        )?;
        // Constrained VBR keeps quality adaptive without letting a loud
        // transient inflate one packet, which would add serialization delay
        // exactly when the audio matters most.
        setting(encoder.set_vbr(true), "vbr")?;
        setting(encoder.set_vbr_constraint(true), "constrained vbr")?;
        // Mid complexity: the encoder runs between two realtime deadlines,
        // so shaving encode time is worth more than the last decibel.
        setting(encoder.set_complexity(OPUS_COMPLEXITY), "complexity")?;
        Ok(Self { encoder })
    }
}

impl AudioEncode for OpusEncoderState {
    fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<usize, RelayError> {
        out.resize(4096, 0);
        match self.encoder.encode_float(pcm, out) {
            Ok(bytes) => {
                out.truncate(bytes);
                Ok(bytes)
            }
            Err(error) => Err(RelayError::Codec(format!("Opus encode: {error}"))),
        }
    }
}

struct OpusDecoderState {
    decoder: opus::Decoder,
    format: AudioFormat,
}

impl OpusDecoderState {
    fn new(format: AudioFormat) -> Result<Self, RelayError> {
        let channels = opus_channels(format.channels)?;
        let decoder = opus::Decoder::new(format.sample_rate, channels)
            .map_err(|error| RelayError::Codec(format!("Opus decoder init: {error}")))?;
        Ok(Self { decoder, format })
    }
}

impl AudioDecode for OpusDecoderState {
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> Result<usize, RelayError> {
        // opus returns samples *per channel*; callers want interleaved total.
        self.decoder
            .decode_float(payload, out, false)
            .map(|per_channel| per_channel * self.format.channels as usize)
            .map_err(|error| RelayError::Codec(format!("Opus decode: {error}")))
    }

    fn conceal(&mut self, out: &mut [f32]) -> Result<usize, RelayError> {
        // An empty payload asks Opus for packet-loss concealment. Keep the
        // request bounded to one frame so a long outage decays to silence.
        let samples = self.format.frame_samples().min(out.len());
        match self.decoder.decode_float(&[], &mut out[..samples], false) {
            Ok(per_channel) => Ok(per_channel * self.format.channels as usize),
            Err(_) => {
                out[..samples].fill(0.0);
                Ok(samples)
            }
        }
    }
}

fn opus_channels(channels: u16) -> Result<opus::Channels, RelayError> {
    match channels {
        1 => Ok(opus::Channels::Mono),
        2 => Ok(opus::Channels::Stereo),
        other => Err(RelayError::Codec(format!(
            "Opus supports 1 or 2 channels, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_geometry() {
        let format = AudioFormat::new(48_000, 1, 20);
        assert_eq!(format.frame_samples(), 960);
        let stereo = AudioFormat::new(48_000, 2, 10);
        assert_eq!(stereo.frame_samples(), 960);
    }

    #[test]
    fn pcm_round_trip() {
        let format = AudioFormat::new(48_000, 1, 20);
        let mut encoder = make_encoder(CodecKind::Pcm, format).unwrap();
        let mut decoder = make_decoder(CodecKind::Pcm, format).unwrap();
        let pcm: Vec<f32> = (0..960).map(|i| (i as f32 * 0.001).sin()).collect();
        let mut payload = Vec::new();
        encoder.encode(&pcm, &mut payload).unwrap();
        let mut out = vec![0.0; 960];
        assert_eq!(decoder.decode(&payload, &mut out).unwrap(), 960);
        assert_eq!(out, pcm);
    }

    #[test]
    fn pcm_frames_must_be_exactly_the_negotiated_size() {
        let format = AudioFormat::new(48_000, 1, 20);
        let mut decoder = make_decoder(CodecKind::Pcm, format).unwrap();
        let mut out = vec![0.0; 960];
        // Short, long, and ragged payloads are all corrupt frames.
        assert!(decoder.decode(&vec![0u8; 960 * 4 - 4], &mut out).is_err());
        assert!(decoder.decode(&vec![0u8; 960 * 4 + 4], &mut out).is_err());
        assert!(decoder.decode(&vec![0u8; 960 * 4 + 3], &mut out).is_err());
        assert!(decoder.decode(&vec![0u8; 960 * 4], &mut out).is_ok());
    }

    #[test]
    fn opus_round_trip_carries_tone() {
        let format = AudioFormat::new(48_000, 1, 20);
        let mut encoder = make_encoder(CodecKind::Opus, format).unwrap();
        let mut decoder = make_decoder(CodecKind::Opus, format).unwrap();
        let pcm: Vec<f32> = (0..960)
            .map(|i| (i as f32 * 440.0 * 2.0 * std::f32::consts::PI / 48_000.0).sin() * 0.5)
            .collect();
        let mut payload = Vec::new();
        let bytes = encoder.encode(&pcm, &mut payload).unwrap();
        assert!(bytes > 0 && bytes < pcm.len() * 4);
        let mut out = vec![0.0; 960];
        assert_eq!(decoder.decode(&payload, &mut out).unwrap(), 960);
        // Opus is lossy; assert energy survives instead of bit equality.
        let energy: f32 = out.iter().map(|s| s * s).sum();
        assert!(energy > 1.0, "decoded frame should carry the tone energy");
    }

    #[test]
    fn opus_concealment_produces_a_frame() {
        let format = AudioFormat::new(48_000, 1, 20);
        let mut decoder = make_decoder(CodecKind::Opus, format).unwrap();
        let mut out = vec![0.0; 960];
        let samples = decoder.conceal(&mut out).unwrap();
        assert!(samples > 0 && samples <= 960);
    }
}
