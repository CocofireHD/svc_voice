//! Sound card capture and playback (`feature = "device"`).
//!
//! The wire side of the client only ever sees one shape: 20 ms of mono f32 at
//! 48 kHz. Real devices are rarely that convenient — 44.1 kHz stereo headsets
//! are the common case — so [`FrameAssembler`] does the downmix and the
//! resample, and both the microphone and the playback queue go through it.
//!
//! Everything below the two thin cpal callbacks is plain data, so it is tested
//! without a sound card.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::FRAME_SAMPLES;

/// Turns whatever a device produces into 48 kHz mono 20 ms frames.
///
/// Input chunks may be any length, any channel count (they are averaged down to
/// mono) and any sample rate (linear interpolation, which is enough for voice:
/// the codec's own bandwidth is far below the resampler's error).
pub struct FrameAssembler {
    in_rate: u32,
    in_channels: usize,
    pending: Vec<f32>,
    phase: f64,
    step: f64,
}

impl FrameAssembler {
    pub fn new(in_rate: u32, in_channels: usize) -> Self {
        let in_channels = in_channels.max(1);
        let rate = in_rate.max(1);
        Self {
            in_rate: rate,
            in_channels,
            pending: Vec::new(),
            phase: 0.0,
            step: rate as f64 / crate::audio::SAMPLE_RATE as f64,
        }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Accept one interleaved chunk from the device callback.
    pub fn push(&mut self, interleaved: &[f32]) {
        if self.in_channels == 1 {
            self.pending.extend_from_slice(interleaved);
            return;
        }
        let ch = self.in_channels;
        self.pending.extend(
            interleaved
                .chunks_exact(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32),
        );
    }

    /// One 20 ms frame at 48 kHz, or `None` while the device has not produced
    /// enough samples yet. Never blocks — it is called from the voice tick.
    pub fn next_frame(&mut self) -> Option<Vec<f32>> {
        let advance = FRAME_SAMPLES as f64 * self.step;
        // Index of the last sample this frame reads; interpolation needs one
        // sample after it only when the rate actually differs.
        let last_needed = self.phase + (FRAME_SAMPLES - 1) as f64 * self.step;
        if (self.pending.len() as f64) < last_needed + 1.0 {
            return None;
        }
        let mut out = Vec::with_capacity(FRAME_SAMPLES);
        for i in 0..FRAME_SAMPLES {
            let idx = self.phase + i as f64 * self.step;
            let i0 = idx.floor() as usize;
            let frac = (idx - i0 as f64) as f32;
            let a = self.pending[i0];
            let b = self.pending.get(i0 + 1).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
        }
        self.phase += advance;
        let consumed = self.phase.floor() as usize;
        if consumed > 0 {
            let n = consumed.min(self.pending.len());
            self.pending.drain(..n);
            self.phase -= consumed as f64;
        }
        Some(out)
    }

    /// How many frames of audio are buffered and ready. A growing backlog means
    /// the tick is slower than the device.
    pub fn buffered_frames(&self) -> usize {
        (self.pending.len() as f64 / self.step / FRAME_SAMPLES as f64) as usize
    }
}

/// Microphone: the default input device, delivering 20 ms frames.
pub struct Capture {
    rx: Receiver<Vec<f32>>,
    assembler: FrameAssembler,
    _stream: cpal::Stream,
}

impl Capture {
    /// Opens the default input at its own rate and channel count.
    pub fn open_default() -> Result<Self, String> {
        Self::open(None)
    }

    /// Opens the input whose name contains `selector` (case-insensitive), or the
    /// default input when `selector` is `None`. Picking by name matters on a
    /// machine with a virtual cable, where the default is not the microphone.
    pub fn open(selector: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = match selector {
            None => host
                .default_input_device()
                .ok_or("no default input device")?,
            Some(needle) => pick_device(host.input_devices(), needle)?,
        };
        let supported = device
            .default_input_config()
            .map_err(|e| format!("input config: {e}"))?;
        let rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let err_fn = |e| eprintln!("[svc-audio] input stream error: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    let _ = tx.send(data.to_vec());
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let _ = tx.send(data.iter().map(|s| *s as f32 / 32768.0).collect());
                },
                err_fn,
                None,
            ),
            other => return Err(format!("unsupported input sample format {other:?}")),
        }
        .map_err(|e| format!("build input stream: {e}"))?;
        stream.play().map_err(|e| format!("start capture: {e}"))?;

        Ok(Self {
            rx,
            assembler: FrameAssembler::new(rate, channels),
            _stream: stream,
        })
    }

    /// Drains everything the device delivered since the last call and returns
    /// the frames that became complete. Call once per voice tick.
    pub fn drain(&mut self) -> Vec<Vec<f32>> {
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => self.assembler.push(&chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        let mut frames = Vec::new();
        while let Some(f) = self.assembler.next_frame() {
            frames.push(f);
        }
        frames
    }

    pub fn input_rate(&self) -> u32 {
        self.assembler.in_rate()
    }
}

/// Playback: a queue the voice tick fills with decoded 48 kHz mono PCM.
pub struct Playback {
    tx: Sender<Vec<f32>>,
    _stream: cpal::Stream,
}

impl Playback {
    pub fn open_default() -> Result<Self, String> {
        Self::open(None)
    }

    /// Opens the output whose name contains `selector`, or the default output.
    pub fn open(selector: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = match selector {
            None => host
                .default_output_device()
                .ok_or("no default output device")?,
            Some(needle) => pick_device(host.output_devices(), needle)?,
        };
        let supported = device
            .default_output_config()
            .map_err(|e| format!("output config: {e}"))?;
        let out_channels = supported.channels() as usize;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        // The callback owns the queue; the tick only ever hands over finished
        // frames, so no allocation and no decoding happens on the audio thread.
        let mut queue: VecDeque<f32> = VecDeque::new();
        let err_fn = |e| eprintln!("[svc-audio] output stream error: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |out: &mut [f32], _| {
                    fill(&mut queue, &rx, out, out_channels, |s| s);
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |out: &mut [i16], _| {
                    fill(&mut queue, &rx, out, out_channels, |s: f32| {
                        (s.clamp(-1.0, 1.0) * 32767.0) as i16
                    });
                },
                err_fn,
                None,
            ),
            other => return Err(format!("unsupported output sample format {other:?}")),
        }
        .map_err(|e| format!("build output stream: {e}"))?;
        stream.play().map_err(|e| format!("start playback: {e}"))?;

        Ok(Self {
            tx,
            _stream: stream,
        })
    }

    /// Queue one frame of 48 kHz mono PCM. Dropped if playback is gone.
    /// Mono PCM: both ears get the same samples.
    pub fn push(&self, pcm: &[f32]) {
        let mut interleaved = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            interleaved.push(*s);
            interleaved.push(*s);
        }
        let _ = self.tx.send(interleaved);
    }

    /// Stereo PCM: `left` and `right` are the two ears, sample by sample.
    pub fn push_stereo(&self, left: &[f32], right: &[f32]) {
        let n = left.len().min(right.len());
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            interleaved.push(left[i]);
            interleaved.push(right[i]);
        }
        let _ = self.tx.send(interleaved);
    }
}

/// First device whose name contains `needle`, case-insensitively.
fn pick_device(
    devices: Result<impl Iterator<Item = cpal::Device>, cpal::DevicesError>,
    needle: &str,
) -> Result<cpal::Device, String> {
    let needle_lower = needle.to_lowercase();
    for device in devices.map_err(|e| format!("list devices: {e}"))? {
        let name = device.name().unwrap_or_default();
        if name.to_lowercase().contains(&needle_lower) {
            eprintln!("[svc-audio] using \"{name}\"");
            return Ok(device);
        }
    }
    Err(format!("no device matching {needle:?}"))
}

/// Shared body of both output callbacks: pull queued PCM, write it to every
/// channel, and output silence when the queue runs dry (never a repeat of the
/// last buffer, which would buzz).
fn fill<T: Copy>(
    queue: &mut VecDeque<f32>,
    rx: &Receiver<Vec<f32>>,
    out: &mut [T],
    channels: usize,
    convert: impl Fn(f32) -> T,
) {
    while let Ok(chunk) = rx.try_recv() {
        queue.extend(chunk);
    }
    for frame in out.chunks_mut(channels) {
        // Frames are queued as (left, right) pairs; a mono device hears the
        // average, a surround device gets silence in the extra channels.
        let left = queue.pop_front().unwrap_or(0.0);
        let right = queue.pop_front().unwrap_or(0.0);
        if channels == 1 {
            let mixed = (left + right) * 0.5;
            frame[0] = convert(mixed);
            continue;
        }
        for (i, s) in frame.iter_mut().enumerate() {
            *s = convert(if i == 0 {
                left
            } else if i == 1 {
                right
            } else {
                0.0
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, freq: f32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn at_48k_a_frame_is_exactly_one_chunk() {
        let mut a = FrameAssembler::new(48_000, 1);
        a.push(&vec![0.1; FRAME_SAMPLES]);
        let f = a.next_frame().expect("one frame");
        assert_eq!(f.len(), FRAME_SAMPLES);
        assert!(a.next_frame().is_none(), "nothing left over");
    }

    #[test]
    fn stereo_is_downmixed_to_mono() {
        let mut a = FrameAssembler::new(48_000, 2);
        let mut inter = Vec::new();
        for _ in 0..FRAME_SAMPLES {
            inter.push(0.5);
            inter.push(-0.5);
        }
        a.push(&inter);
        let f = a.next_frame().unwrap();
        assert_eq!(f.len(), FRAME_SAMPLES);
        assert!(f.iter().all(|s| s.abs() < 1e-6), "L and R cancel exactly");
    }

    #[test]
    fn rate_44k1_is_resampled_to_the_wire_rate() {
        let mut a = FrameAssembler::new(44_100, 1);
        a.push(&tone(44_100, 440.0, 1.0));
        let mut frames = 0;
        while a.next_frame().is_some() {
            frames += 1;
        }
        // 1 s at 48 kHz is 50 frames, and the last fraction is withheld until
        // the interpolation point exists.
        assert!((49..=50).contains(&frames), "got {frames} frames");
    }

    #[test]
    fn a_resampled_tone_keeps_its_frequency() {
        let mut a = FrameAssembler::new(44_100, 2);
        let mono = tone(44_100, 700.0, 1.0);
        let inter: Vec<f32> = mono.iter().flat_map(|s| [*s, *s]).collect();
        a.push(&inter);
        let mut pcm = Vec::new();
        while let Some(f) = a.next_frame() {
            pcm.extend(f);
        }
        let dom = crate::audio::dominant_freq(&pcm[FRAME_SAMPLES..], 300.0, 1200.0);
        assert!(
            (dom - 700.0).abs() <= 8.0,
            "dominant {dom} Hz, expected 700"
        );
        assert!(crate::audio::rms(&pcm) > 0.1, "rms {}", crate::audio::rms(&pcm));
    }

    #[test]
    fn playback_silence_does_not_repeat_the_last_buffer() {
        let (_tx, rx) = mpsc::channel::<Vec<f32>>();
        let mut queue = VecDeque::new();
        let mut out = vec![0.7f32; 480];
        fill(&mut queue, &rx, &mut out, 2, |s| s);
        assert!(out.iter().all(|s| *s == 0.0), "an empty queue is silent");
    }

    #[test]
    fn playback_keeps_stereo_pairs_and_downmixes_for_mono() {
        // Queued frames are (left, right) pairs: a stereo device gets them
        // untouched, a mono device hears their average.
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        tx.send(vec![0.5, 0.0, -0.5, 0.25]).unwrap();
        let mut queue = VecDeque::new();
        let mut out = [0.0f32; 4];
        fill(&mut queue, &rx, &mut out, 2, |s| s);
        assert_eq!(out, [0.5, 0.0, -0.5, 0.25]);

        let (tx2, rx2) = mpsc::channel::<Vec<f32>>();
        tx2.send(vec![0.5, 0.0]).unwrap();
        let mut queue2 = VecDeque::new();
        let mut mono = [0.0f32; 1];
        fill(&mut queue2, &rx2, &mut mono, 1, |s| s);
        assert_eq!(mono, [0.25]);
    }
}
