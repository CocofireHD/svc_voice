//! `svc-audio` — the PCM side of the voice client.
//!
//! `svc-client` moves opaque Opus payloads; this crate turns microphone PCM
//! into those payloads and back. It is deliberately small and dependency-free
//! apart from a pure-Rust Opus codec (`opus-rs`, RFC 6716), so it builds on
//! every platform Horizon ships on and inside a native mod without a C
//! toolchain or a system `libopus`.
//!
//! Wire contract (§5.1): 20 ms frames, 48 kHz, mono, 960 samples per frame.
//! The whole voice chat uses that one cadence, so no resampling and no
//! variable frame sizes are needed here.
//!
//! ```no_run
//! use svc_engine::audio::{VoiceDecoder, VoiceEncoder};
//!
//! let mut enc = VoiceEncoder::new().expect("encoder");
//! let mut dec = VoiceDecoder::new().expect("decoder");
//! let pcm = svc_engine::audio::tone(440.0, 960);
//! let packet = enc.encode(&pcm).expect("encode");
//! let back = dec.decode(Some(&packet)).expect("decode");
//! assert_eq!(back.len(), 960);
//! ```

#[cfg(feature = "device")]
mod device;
pub mod record;
#[cfg(feature = "device")]
pub use device::{Capture, FrameAssembler, Playback};
pub use record::Recorder;

use crate::opus::{Application, OpusDecoder, OpusEncoder};

/// Samples per 20 ms frame at 48 kHz (§5.1).
pub const FRAME_SAMPLES: usize = 960;
/// Sample rate of every voice chat packet.
pub const SAMPLE_RATE: u32 = 48_000;
/// A packet never exceeds this on the wire (§3.3); the codec must fit it.
pub const MAX_PACKET: usize = 1275;

/// PCM encoder: 20 ms of mono f32 (`-1.0..=1.0`) → one Opus packet.
pub struct VoiceEncoder {
    inner: OpusEncoder,
    scratch: [f32; FRAME_SAMPLES],
    scratch_len: usize,
    packets: u64,
}

impl VoiceEncoder {
    pub fn new() -> Result<Self, String> {
        // Voip: the mode libopus uses for speech at low bitrate; it is also what
        // the Java client asks for, so the server and the other clients see the
        // same kind of stream they expect.
        let inner = OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Voip)
            .map_err(|e| format!("opus encoder: {e}"))?;
        Ok(Self {
            inner,
            scratch: [0.0; FRAME_SAMPLES],
            scratch_len: 0,
            packets: 0,
        })
    }

    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Target bitrate in bit/s. The codec's own default is 64 kb/s; Simple Voice
    /// Chat's default configuration sits well below that, and a lower rate keeps a
    /// 20 ms frame comfortably inside the 1275-byte MTU of §3.3.
    pub fn with_bitrate(mut self, bps: i32) -> Self {
        self.inner.bitrate_bps = bps;
        self
    }

    /// Encode exactly 20 ms. A short slice is zero-padded, a long one truncated,
    /// so a caller that hands over whatever the microphone produced cannot
    /// desynchronise the stream.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String> {
        let n = pcm.len().min(FRAME_SAMPLES);
        self.scratch[..n].copy_from_slice(&pcm[..n]);
        for s in &mut self.scratch[n..] {
            *s = 0.0;
        }
        self.scratch_len = FRAME_SAMPLES;
        let mut out = [0u8; MAX_PACKET];
        let len = self
            .inner
            .encode(&self.scratch[..FRAME_SAMPLES], FRAME_SAMPLES, &mut out)
            .map_err(|e| format!("opus encode: {e}"))?;
        self.packets += 1;
        Ok(out[..len].to_vec())
    }
}

/// Opus decoder back to 20 ms of mono f32.
pub struct VoiceDecoder {
    inner: OpusDecoder,
    packets: u64,
    concealed: u64,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, String> {
        let inner =
            OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(|e| format!("opus decoder: {e}"))?;
        Ok(Self {
            inner,
            packets: 0,
            concealed: 0,
        })
    }

    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Frames the codec had to invent because a packet was missing.
    pub fn concealed(&self) -> u64 {
        self.concealed
    }

    /// Decode one packet. A `None` (a gap in the sequence) asks the codec for
    /// packet-loss concealment, which is what libopus does and what keeps a
    /// dropped frame from clicking (§9).
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<Vec<f32>, String> {
        let mut out = vec![0.0f32; FRAME_SAMPLES];
        let input: &[u8] = packet.unwrap_or(&[]);
        if packet.is_none() {
            self.concealed += 1;
        }
        let n = self
            .inner
            .decode(input, FRAME_SAMPLES, &mut out)
            .map_err(|e| format!("opus decode: {e}"))?;
        self.packets += 1;
        out.truncate(n);
        Ok(out)
    }
}

/// A sine tone as 20 ms frames would carry it, for tests and for the probe.
pub fn tone(freq: f32, samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            (2.0 * std::f32::consts::PI * freq * t).sin() * 0.25
        })
        .collect()
}

/// Continues the phase of [`tone`] across frames (frame `index` starts at
/// `index * FRAME_SAMPLES`), so a long tone has no click at frame boundaries.
pub fn tone_frame(freq: f32, index: u64) -> Vec<f32> {
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = (index as usize * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            (2.0 * std::f32::consts::PI * freq * t).sin() * 0.25
        })
        .collect()
}

/// Root mean square, the cheap "is there audio at all" measure.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Energy at one frequency (Goertzel), normalised by the sample count so two
/// windows of different length are comparable.
pub fn goertzel(samples: &[f32], freq: f32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE as f32;
    let coeff = 2.0 * k.cos();
    let mut s0;
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power.max(0.0) / n as f32
}

/// The strongest frequency between `lo` and `hi`, in 2 Hz steps.
pub fn dominant_freq(samples: &[f32], lo: f32, hi: f32) -> f32 {
    let mut best = (0.0f32, lo);
    let mut f = lo;
    while f <= hi {
        let p = goertzel(samples, f);
        if p > best.0 {
            best = (p, f);
        }
        f += 2.0;
    }
    best.1
}

/// How much of the energy sits at `freq` compared with the rest of the band
/// around it: a cheap, honest "did the tone survive the codec and the server".
pub fn tone_contrast(samples: &[f32], freq: f32) -> f32 {
    let at = goertzel(samples, freq);
    let others: f32 = [freq - 200.0, freq - 120.0, freq + 120.0, freq + 200.0]
        .iter()
        .map(|f| goertzel(samples, *f))
        .sum::<f32>()
        / 4.0;
    if others <= f32::EPSILON {
        f32::INFINITY
    } else {
        at / others
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_produces_packets_that_fit_the_mtu() {
        let mut enc = VoiceEncoder::new().unwrap().with_bitrate(24_000);
        let pcm = tone_frame(440.0, 0);
        let pkt = enc.encode(&pcm).unwrap();
        assert!(!pkt.is_empty(), "an encoded frame is never empty");
        assert!(
            pkt.len() <= MAX_PACKET,
            "packet {} B exceeds MTU",
            pkt.len()
        );
        assert!(pkt.len() < 200, "a 20 ms frame at 24 kb/s: {} B", pkt.len());
    }

    #[test]
    fn a_tone_survives_encode_and_decode() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut pcm = Vec::new();
        for i in 0..50u64 {
            let frame = tone_frame(440.0, i);
            let pkt = enc.encode(&frame).unwrap();
            pcm.extend(dec.decode(Some(&pkt)).unwrap());
        }
        assert_eq!(pcm.len(), 50 * FRAME_SAMPLES);
        let dom = dominant_freq(&pcm[FRAME_SAMPLES * 5..], 200.0, 900.0);
        assert!(
            (dom - 440.0).abs() <= 6.0,
            "dominant frequency {dom} Hz, expected ~440"
        );
        assert!(
            tone_contrast(&pcm[FRAME_SAMPLES * 5..], 440.0) > 20.0,
            "the tone must stand out from its neighbours, contrast {}",
            tone_contrast(&pcm[FRAME_SAMPLES * 5..], 440.0)
        );
        assert!(
            rms(&pcm) > 0.05,
            "decoded audio is audible, rms {}",
            rms(&pcm)
        );
    }

    #[test]
    fn silence_stays_silent() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut pcm = Vec::new();
        for _ in 0..10 {
            let pkt = enc.encode(&[0.0; FRAME_SAMPLES]).unwrap();
            pcm.extend(dec.decode(Some(&pkt)).unwrap());
        }
        assert!(
            rms(&pcm) < 0.01,
            "digital silence decodes quiet, rms {}",
            rms(&pcm)
        );
    }

    #[test]
    fn a_lost_packet_is_concealed_not_fatal() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let first = enc.encode(&tone_frame(300.0, 0)).unwrap();
        let _second = enc.encode(&tone_frame(300.0, 1)).unwrap();
        let third = enc.encode(&tone_frame(300.0, 2)).unwrap();
        dec.decode(Some(&first)).unwrap();
        let concealed = dec.decode(None).unwrap();
        assert_eq!(
            concealed.len(),
            FRAME_SAMPLES,
            "a gap still yields one frame"
        );
        dec.decode(Some(&third)).unwrap();
        assert_eq!(dec.concealed(), 1, "exactly one frame was concealed");
        assert_eq!(dec.packets(), 3);
    }

    #[test]
    fn short_and_long_input_are_normalised_to_one_frame() {
        let mut enc = VoiceEncoder::new().unwrap();
        enc.encode(&[0.1; 10]).unwrap();
        enc.encode(&[0.1; 5000]).unwrap();
        assert_eq!(enc.packets(), 2);
        let mut dec = VoiceDecoder::new().unwrap();
        let pkt = enc.encode(&[0.0; 1]).unwrap();
        assert_eq!(dec.decode(Some(&pkt)).unwrap().len(), FRAME_SAMPLES);
    }
}
