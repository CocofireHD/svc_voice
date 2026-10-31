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

/// What opens the microphone. The original sends only while the player talks
/// or holds the key; sending silence costs bandwidth and hides nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Always on, the first releases' behaviour.
    #[default]
    Open,
    /// Open while the frame's level passes the threshold, plus a hangover.
    VoiceActivity,
    /// Open only while the key is held.
    PushToTalk,
}

/// Root-mean-square of a frame, the level the gate compares.
pub fn frame_level(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f32 = pcm.iter().map(|s| s * s).sum();
    (sum / pcm.len() as f32).sqrt()
}

/// Whether the microphone sends this frame. Pure, so the rule is testable
/// without a socket or a sound card.
pub fn gate_opens(
    gate: Gate,
    level: f32,
    threshold: f32,
    since_loud: Option<std::time::Duration>,
    hangover: std::time::Duration,
    ptt: bool,
) -> bool {
    match gate {
        Gate::Open => true,
        Gate::VoiceActivity => {
            level >= threshold || since_loud.is_some_and(|d| d < hangover)
        }
        Gate::PushToTalk => ptt,
    }
}

pub struct PipelineConfig {
    /// Opus target bitrate in bit/s.
    pub bitrate: i32,
    pub jitter: JitterConfig,
    /// When the microphone is allowed to send.
    pub gate: Gate,
    /// Level (RMS of the 20 ms frame) that counts as speech, 0..1.
    pub threshold: f32,
    /// How long the gate stays open after the last loud frame.
    pub hangover: std::time::Duration,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            bitrate: 24_000,
            jitter: JitterConfig::default(),
            gate: Gate::Open,
            threshold: 0.012,
            hangover: std::time::Duration::from_millis(300),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Microphone frames handed to the session.
    pub mic_frames: u64,
    /// Frames the gate kept back because nobody was talking.
    pub mic_skipped: u64,
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
    /// Sounds played quieter than full volume: proves the distance curve ran.
    attenuated: u64,
    /// Whether the last frame we sent opened the stream (for the stop packet).
    sending: bool,
    /// Last frame that passed the threshold, for the hangover.
    last_loud: Option<Instant>,
    /// Push-to-talk key, set from the game thread.
    ptt: bool,
    /// Level of the last frame, for logging.
    level: f32,
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
            attenuated: 0,
            sending: false,
            last_loud: None,
            ptt: false,
            level: 0.0,
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

    /// Choose what opens the microphone.
    pub fn set_gate(&mut self, gate: Gate) {
        if self.cfg.gate != gate {
            self.cfg.gate = gate;
            self.sending = false;
            self.last_loud = None;
        }
    }

    pub fn gate(&self) -> Gate {
        self.cfg.gate
    }

    /// The push-to-talk key, as last published by the game thread.
    pub fn set_ptt(&mut self, held: bool) {
        self.ptt = held;
    }

    /// Level of the last captured frame, 0..1.
    pub fn level(&self) -> f32 {
        self.level
    }

    /// Sounds delivered with a distance gain below 1.0 (§7.4).
    pub fn attenuated(&self) -> u64 {
        self.attenuated
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
            let level = frame_level(&frame);
            self.level = level;
            let now = Instant::now();
            if level >= self.cfg.threshold {
                self.last_loud = Some(now);
            }
            let open = gate_opens(
                self.cfg.gate,
                level,
                self.cfg.threshold,
                self.last_loud.map(|t| now.duration_since(t)),
                self.cfg.hangover,
                self.ptt,
            );
            if !open {
                // Tell the server the stream stopped once, then stay quiet:
                // §6.6 reads an empty payload as "speaker stopped".
                if self.sending {
                    match client.push_mic(&[], false) {
                        Ok(()) => self.sending = false,
                        Err(_) => self.stats.mic_rejected += 1,
                    }
                }
                self.stats.mic_skipped += 1;
                continue;
            }
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
                    self.sending = true;
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

    /// `tick`, but each sound is attenuated by the speaker's distance from
    /// `listener` (§7.4). Positions come from the caller because the voice
    /// packet only carries the sender's UUID; `position_of` maps that to a
    /// world position. A speaker we cannot place is played at full volume
    /// rather than dropped — silence would be a worse guess.
    pub fn tick_positional<F>(
        &mut self,
        now: Instant,
        client: &mut ClientSession,
        mic: &mut dyn MicSource,
        sink: &dyn SpeakerSink,
        listener: [f64; 3],
        position_of: F,
    ) -> usize
    where
        F: Fn(Uuid) -> Option<[f64; 3]>,
    {
        let mut played = 0;
        let events: Vec<Incoming> = match self.inbox.as_ref() {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for event in events {
            let Incoming::Sound(sound) = event else {
                continue;
            };
            // §5.3: group audio is not positional.
            if sound.group {
                played += self.deliver(now, &sound, sink);
                continue;
            }
            let speaker = sound.position.or_else(|| position_of(sound.sender));
            let gain = match speaker {
                Some(at) => match distance_gain(listener, at, sound.distance) {
                    Some(gain) => gain,
                    // §7.4: beyond the radius this client plays nothing.
                    None => continue,
                },
                None => 1.0,
            };
            if gain < 0.999 {
                self.attenuated += 1;
            }
            played += self.deliver_with_gain(now, &sound, gain, sink);
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
    fn voice_activity_opens_on_speech_and_closes_on_silence() {
        let hang = Duration::from_millis(300);
        // Loud enough: open.
        assert!(gate_opens(Gate::VoiceActivity, 0.2, 0.012, None, hang, false));
        // Quiet, never loud: closed.
        assert!(!gate_opens(Gate::VoiceActivity, 0.001, 0.012, None, hang, false));
        // Quiet, but the last loud frame is inside the hangover: still open.
        assert!(gate_opens(
            Gate::VoiceActivity,
            0.001,
            0.012,
            Some(Duration::from_millis(120)),
            hang,
            false
        ));
        // The hangover ran out: closed.
        assert!(!gate_opens(
            Gate::VoiceActivity,
            0.001,
            0.012,
            Some(Duration::from_millis(400)),
            hang,
            false
        ));
    }

    #[test]
    fn push_to_talk_ignores_the_level_in_both_directions() {
        let hang = Duration::from_millis(300);
        // Quiet but held: open — that is the point of the key.
        assert!(gate_opens(Gate::PushToTalk, 0.0, 0.012, None, hang, true));
        // Loud but released: closed.
        assert!(!gate_opens(Gate::PushToTalk, 0.9, 0.012, None, hang, false));
    }

    #[test]
    fn the_level_is_the_root_mean_square() {
        assert_eq!(frame_level(&[]), 0.0);
        assert!((frame_level(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert!((frame_level(&[0.0; 960])).abs() < 1e-9);
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
