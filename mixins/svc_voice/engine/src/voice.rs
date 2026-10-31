//! `svc-voice` — the glue between the sound card and the wire.
//!
//! `svc-client` owns the UDP session, `svc-audio` owns Opus, `svc-protocol` owns
//! the jitter buffer. This crate is the loop that runs once per voice tick:
//!
//! ```text
//! microphone -> 20 ms frame -> Opus -> ClientSession::push_mic
//! Incoming::Sound -> JitterBuffer -> Opus decode (+PLC) -> playback queue
//! ```
//!
//! Both ends are traits ([`MicSource`], [`SpeakerSink`]) so the loop can be
//! tested without a sound card, and so a game can feed it from its own audio
//! backend instead of cpal.

use std::collections::HashMap;
use std::time::Instant;

use crate::audio::{VoiceDecoder, VoiceEncoder};
use crate::client::session::{ClientSession, Incoming, SoundFrame};
use crate::protocol::jitter::{JitterBuffer, JitterConfig};
use crate::protocol::wire::Uuid;

/// A source of microphone audio: any number of 20 ms frames per call.
pub trait MicSource {
    fn drain(&mut self) -> Vec<Vec<f32>>;
}

/// Where decoded audio goes.
pub trait SpeakerSink {
    fn push(&self, pcm: &[f32]);
}

#[cfg(feature = "device")]
impl MicSource for crate::audio::Capture {
    fn drain(&mut self) -> Vec<Vec<f32>> {
        crate::audio::Capture::drain(self)
    }
}

#[cfg(feature = "device")]
impl SpeakerSink for crate::audio::Playback {
    fn push(&self, pcm: &[f32]) {
        crate::audio::Playback::push(self, pcm)
    }
}

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Opus target bitrate in bit/s.
    pub bitrate: i32,
    pub jitter: JitterConfig,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            bitrate: 24_000,
            jitter: JitterConfig::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Microphone frames handed to the session.
    pub mic_frames: u64,
    /// Frames the session refused (stale, not connected, outbox full).
    pub mic_rejected: u64,
    /// Frames decoded and queued for playback.
    pub played: u64,
    /// Frames the codec had to invent for a gap (PLC).
    pub concealed: u64,
    /// Streams restarted because the gap was too large to conceal.
    pub resets: u64,
}

/// Distance volume for positional audio (§7.4).
///
/// `v = 1 − min(d, maxDistance) / maxDistance`, and a sound farther than
/// `maxDistance + 1` is dropped entirely. `maxDistance` is the packet's
/// `distance` field, never a hard-coded 48 (§7.4: "A client must not hard-code
/// 48").
///
/// Returns `None` when the sound must be dropped.
pub fn distance_gain(listener: [f64; 3], sound: [f64; 3], max_distance: f32) -> Option<f32> {
    if !(max_distance > 0.0) {
        return Some(1.0);
    }
    let d = ((listener[0] - sound[0]).powi(2)
        + (listener[1] - sound[1]).powi(2)
        + (listener[2] - sound[2]).powi(2))
    .sqrt();
    if d > max_distance as f64 + 1.0 {
        return None;
    }
    let v = 1.0 - (d.min(max_distance as f64) / max_distance as f64) as f32;
    Some(v.clamp(0.0, 1.0))
}

/// Scale PCM in place, saturating at ±1.0 so a loud mix cannot clip into noise.
pub fn apply_gain(pcm: &mut [f32], gain: f32) {
    if (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    for s in pcm.iter_mut() {
        *s = (*s * gain).clamp(-1.0, 1.0);
    }
}

/// One per client. Owns the codec state and one jitter buffer per audio channel.
pub struct VoicePipeline {
    encoder: VoiceEncoder,
    decoder: VoiceDecoder,
    jitter: HashMap<Uuid, JitterBuffer>,
    cfg: PipelineConfig,
    stats: Stats,
    /// Taken from the session once, because `ClientSession::take_inbox` hands the
    /// receiver out permanently.
    inbox: Option<std::sync::mpsc::Receiver<Incoming>>,
}

impl VoicePipeline {
    pub fn new(cfg: PipelineConfig) -> Result<Self, String> {
        Ok(Self {
            encoder: VoiceEncoder::new()?.with_bitrate(cfg.bitrate),
            decoder: VoiceDecoder::new()?,
            jitter: HashMap::new(),
            cfg,
            stats: Stats::default(),
            inbox: None,
        })
    }

    /// Bind the pipeline to a session's inbound events. Call once, after
    /// `ClientSession::connect` — the session's receiver can only be taken once.
    pub fn attach(&mut self, client: &mut ClientSession) {
        self.inbox = client.take_inbox();
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn config(&self) -> &PipelineConfig {
        &self.cfg
    }

    /// Encode one frame of microphone PCM. Public so a caller that batches
    /// frames itself (or an offline test) can use the codec directly.
    pub fn encode_frame(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String> {
        self.encoder.encode(pcm)
    }

    /// Take everything the microphone produced since the last tick and send it.
    /// Returns how many frames reached the session.
    pub fn capture(&mut self, mic: &mut dyn MicSource, client: &mut ClientSession) -> usize {
        let mut sent = 0;
        for frame in mic.drain() {
            let packet = match self.encode_frame(&frame) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[svc-voice] encode: {e}");
                    continue;
                }
            };
            match client.push_mic(&packet, false) {
                Ok(()) => {
                    self.stats.mic_frames += 1;
                    sent += 1;
                }
                Err(_) => self.stats.mic_rejected += 1,
            }
        }
        sent
    }

    /// Feed one relayed sound packet through the jitter buffer and play what
    /// became ready. Returns the number of frames queued for playback.
    pub fn deliver(&mut self, now: Instant, sound: &SoundFrame, sink: &dyn SpeakerSink) -> usize {
        self.deliver_with_gain(now, sound, 1.0, sink)
    }

    /// Same, with a distance gain from [`distance_gain`] applied to every decoded
    /// frame. A gain of 0 means the speaker is at the edge of audibility.
    pub fn deliver_with_gain(
        &mut self,
        now: Instant,
        sound: &SoundFrame,
        gain: f32,
        sink: &dyn SpeakerSink,
    ) -> usize {
        let buffer = self
            .jitter
            .entry(sound.channel_id)
            .or_insert_with(|| JitterBuffer::new(self.cfg.jitter.clone()));
        let deliveries = buffer.push(now, sound.sequence, &sound.data);
        let mut played = 0;
        for delivery in deliveries {
            if delivery.reset {
                // The gap is too large to conceal (§6.6): drop the codec state
                // instead of inventing a second of noise.
                match VoiceDecoder::new() {
                    Ok(d) => self.decoder = d,
                    Err(e) => eprintln!("[svc-voice] decoder reset: {e}"),
                }
                self.stats.resets += 1;
                if delivery.data.is_empty() {
                    continue;
                }
            }
            for _ in 0..delivery.lost {
                match self.decoder.decode(None) {
                    Ok(mut pcm) => {
                        apply_gain(&mut pcm, gain);
                        sink.push(&pcm);
                        self.stats.concealed += 1;
                        self.stats.played += 1;
                        played += 1;
                    }
                    Err(e) => eprintln!("[svc-voice] conceal: {e}"),
                }
            }
            if delivery.is_stop_frame() {
                continue;
            }
            match self.decoder.decode(Some(&delivery.data)) {
                Ok(mut pcm) => {
                    apply_gain(&mut pcm, gain);
                    sink.push(&pcm);
                    self.stats.played += 1;
                    played += 1;
                }
                Err(e) => eprintln!("[svc-voice] decode: {e}"),
            }
        }
        played
    }

    /// One voice tick: drain the session, play what arrived, then send the mic.
    /// `now` is the tick's timestamp — pass one `Instant` per tick so every
    /// channel sees a consistent clock.
    pub fn tick(
        &mut self,
        now: Instant,
        client: &mut ClientSession,
        mic: &mut dyn MicSource,
        sink: &dyn SpeakerSink,
    ) -> usize {
        let mut played = 0;
        // Collect first: the receiver borrow must end before `deliver` needs
        // `&mut self`. A tick carries a handful of events, not a stream.
        let events: Vec<Incoming> = match self.inbox.as_ref() {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for event in events {
            if let Incoming::Sound(sound) = event {
                played += self.deliver(now, &sound, sink);
            }
        }
        self.capture(mic, client);
        played
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use super::*;
    use crate::audio::FRAME_SAMPLES;

    struct TestMic {
        frames: Vec<Vec<f32>>,
    }

    impl MicSource for TestMic {
        fn drain(&mut self) -> Vec<Vec<f32>> {
            std::mem::take(&mut self.frames)
        }
    }

    #[test]
    fn the_microphone_source_is_drained_completely() {
        let mut mic = TestMic {
            frames: vec![vec![0.1; FRAME_SAMPLES]; 3],
        };
        assert_eq!(mic.drain().len(), 3);
        assert!(mic.drain().is_empty(), "a drained source yields nothing");
    }

    #[derive(Default)]
    struct TestSink {
        pcm: RefCell<Vec<Vec<f32>>>,
    }

    impl SpeakerSink for TestSink {
        fn push(&self, pcm: &[f32]) {
            self.pcm.borrow_mut().push(pcm.to_vec());
        }
    }

    impl TestSink {
        fn frames(&self) -> usize {
            self.pcm.borrow().len()
        }
        fn flat(&self) -> Vec<f32> {
            self.pcm.borrow().iter().flatten().copied().collect()
        }
    }

    fn sound(pipeline: &mut VoicePipeline, seq: i64, index: u64) -> SoundFrame {
        let data = pipeline
            .encode_frame(&crate::audio::tone_frame(440.0, index))
            .unwrap();
        SoundFrame {
            channel_id: Uuid::from_u128(7),
            sender: Uuid::from_u128(9),
            data,
            sequence: seq,
            distance: 48.0,
            position: None,
            group: false,
            whispering: false,
            category: None,
        }
    }

    #[test]
    fn decoded_audio_reaches_the_sink_in_order() {
        let mut p = VoicePipeline::new(PipelineConfig::default()).unwrap();
        let sink = TestSink::default();
        let now = Instant::now();
        for i in 0..5u64 {
            let s = sound(&mut p, i as i64, i);
            assert_eq!(p.deliver(now, &s, &sink), 1, "in-order frame plays at once");
        }
        assert_eq!(sink.frames(), 5);
        assert_eq!(sink.flat().len(), 5 * FRAME_SAMPLES);
        assert_eq!(p.stats().played, 5);
        assert_eq!(p.stats().concealed, 0);

        let dom = crate::audio::dominant_freq(&sink.flat()[FRAME_SAMPLES..], 200.0, 900.0);
        assert!((dom - 440.0).abs() <= 6.0, "dominant {dom} Hz");
    }

    #[test]
    fn a_small_gap_is_concealed_and_playback_continues() {
        let mut p = VoicePipeline::new(PipelineConfig::default()).unwrap();
        let sink = TestSink::default();
        let now = Instant::now();
        // 0 plays, then 1 is missing: pushing 2..4 buffers until the threshold,
        // which flushes sequence 2 with one concealed frame in front of it.
        let first = sound(&mut p, 0, 0);
        p.deliver(now + Duration::from_millis(1), &first, &sink);
        for seq in 2..6u64 {
            let s = sound(&mut p, seq as i64, seq);
            p.deliver(now + Duration::from_millis(2 + seq), &s, &sink);
        }
        // 4 frames arrived (0,2,3,4) and one gap was filled by the codec, so the
        // sink saw at least as many frames as arrived, in ascending order.
        assert!(p.stats().played >= 4, "{:?}", p.stats());
        assert!(
            p.stats().concealed >= 1,
            "the hole must be concealed: {:?}",
            p.stats()
        );
        assert!(sink.frames() >= 5, "{} frames", sink.frames());
        assert!(
            sink.flat().iter().any(|s| s.abs() > 0.05),
            "concealed frames are not silence"
        );
    }

    #[test]
    fn a_stop_frame_resets_without_playing() {
        let mut p = VoicePipeline::new(PipelineConfig::default()).unwrap();
        let sink = TestSink::default();
        let now = Instant::now();
        let head = sound(&mut p, 0, 0);
        p.deliver(now, &head, &sink);
        let before = p.stats();
        let stop = SoundFrame {
            channel_id: Uuid::from_u128(7),
            sender: Uuid::from_u128(9),
            data: Vec::new(),
            sequence: 1,
            distance: 48.0,
            position: None,
            group: false,
            whispering: false,
            category: None,
        };
        assert_eq!(
            p.deliver(now, &stop, &sink),
            0,
            "nothing is played for a stop"
        );
        assert_eq!(p.stats().played, before.played, "and nothing is counted");
        assert_eq!(
            p.stats().resets,
            before.resets + 1,
            "but the decoder resets"
        );
    }

    #[test]
    fn distance_gain_follows_the_spec_curve() {
        let at = |d: f64| distance_gain([0.0; 3], [d, 0.0, 0.0], 48.0);
        assert_eq!(at(0.0), Some(1.0));
        let half = at(24.0).unwrap();
        assert!(
            (half - 0.5).abs() < 1e-5,
            "half distance is half volume: {half}"
        );
        assert!(at(47.0).unwrap() < 0.05, "{:?}", at(47.0));
        assert_eq!(at(48.0), Some(0.0), "the edge is silent, not dropped");
        assert_eq!(at(49.0), Some(0.0), "the +1 grace block is still kept");
        assert_eq!(
            at(49.5),
            None,
            "beyond maxDistance + 1 the sound is dropped"
        );
        // The radius comes from the packet, never from a constant.
        assert_eq!(distance_gain([0.0; 3], [30.0, 0.0, 0.0], 24.0), None);
        let whisper = distance_gain([0.0; 3], [12.0, 0.0, 0.0], 24.0).unwrap();
        assert!(
            (whisper - 0.5).abs() < 1e-5,
            "whisper radius half: {whisper}"
        );
        // Height counts like any other axis (Euclidean, not horizontal).
        let above = distance_gain([0.0; 3], [0.0, 40.0, 0.0], 48.0).unwrap();
        assert!((above - (1.0 - 40.0 / 48.0)).abs() < 1e-5, "{above}");
    }

    #[test]
    fn gain_scales_and_saturates() {
        let mut pcm = vec![0.8, -0.8];
        apply_gain(&mut pcm, 0.5);
        assert!((pcm[0] - 0.4).abs() < 1e-6 && (pcm[1] + 0.4).abs() < 1e-6);
        let mut loud = vec![0.9, -1.4];
        apply_gain(&mut loud, 2.0);
        assert_eq!(loud, vec![1.0, -1.0], "clipped, not wrapped");
        let mut untouched = vec![0.3];
        apply_gain(&mut untouched, 1.0);
        assert_eq!(untouched, vec![0.3]);
    }

    #[test]
    fn a_gained_delivery_is_quieter_on_the_sink() {
        let mut p = VoicePipeline::new(PipelineConfig::default()).unwrap();
        let sink = TestSink::default();
        let now = Instant::now();
        let loud = sound(&mut p, 0, 0);
        p.deliver(now, &loud, &sink);
        let full = crate::audio::rms(&sink.flat());
        let quiet = {
            let mut q = VoicePipeline::new(PipelineConfig::default()).unwrap();
            let s2 = TestSink::default();
            let frame = sound(&mut q, 0, 1);
            q.deliver_with_gain(now, &frame, 0.25, &s2);
            crate::audio::rms(&s2.flat())
        };
        assert!(quiet < full * 0.5, "quiet {quiet:.3} vs full {full:.3}");
        assert!(
            quiet > 0.01,
            "a quarter volume is still audible: {quiet:.3}"
        );
    }

    #[test]
    fn separate_channels_get_separate_buffers() {
        let mut p = VoicePipeline::new(PipelineConfig::default()).unwrap();
        let sink = TestSink::default();
        let now = Instant::now();
        let mut a = sound(&mut p, 0, 0);
        let mut b = sound(&mut p, 0, 1);
        a.channel_id = Uuid::from_u128(1);
        b.channel_id = Uuid::from_u128(2);
        assert_eq!(p.deliver(now, &a, &sink), 1);
        assert_eq!(
            p.deliver(now, &b, &sink),
            1,
            "a second channel starts at 0 too"
        );
        assert_eq!(sink.frames(), 2);
    }
}
