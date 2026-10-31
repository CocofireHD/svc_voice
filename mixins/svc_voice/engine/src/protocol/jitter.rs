//! Per-speaker jitter buffer for incoming sound packets (SPEC §6.6).
//!
//! The buffer is driven purely by `sequenceNumber`; it never blocks and never
//! waits on a timer — the caller pushes a datagram and immediately gets back
//! the frames that may be decoded now, so a UDP loop stays non-blocking.
//!
//! Rules reproduced from §6.6:
//! * `sequence <= last` is stale and dropped;
//! * `last + 1` (or the very first frame) is delivered immediately, then any
//!   buffered contiguous frames follow;
//! * otherwise the frame is buffered and sorted; the buffer is flushed when it
//!   holds more than `threshold` frames;
//! * on a flush the gap `seq − (last+1)` is reported as `lost` (packet-loss
//!   concealment) unless it exceeds `max_gap_fill`, in which case the decoder
//!   is reset instead;
//! * an empty payload means "speaker stopped": reset everything;
//! * a channel idle for `idle_timeout` may be closed.
//!
//! All sizes are explicit and hard: `capacity` bounds memory, `threshold`
//! bounds latency, `max_gap_fill` bounds PLC work.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Defaults from `ClientConfig` (§10): threshold 3, PLC gap limit 5,
/// channel idle timeout 30 000 ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitterConfig {
    /// Hard cap on buffered frames (memory bound).
    pub capacity: usize,
    /// Flush as soon as more than this many frames are buffered.
    pub threshold: usize,
    /// Maximum number of concealed frames to request for one gap.
    pub max_gap_fill: u32,
    /// A channel without packets for this long is considered dead.
    pub idle_timeout: Duration,
}

impl Default for JitterConfig {
    fn default() -> Self {
        JitterConfig {
            capacity: 16,
            threshold: 3,
            max_gap_fill: 5,
            idle_timeout: Duration::from_millis(30_000),
        }
    }
}

impl JitterConfig {
    /// Clamp nonsensical combinations instead of panicking.
    fn sanitised(&self) -> JitterConfig {
        let capacity = self.capacity.max(1);
        let threshold = self.threshold.min(capacity);
        JitterConfig {
            capacity,
            threshold,
            max_gap_fill: self.max_gap_fill,
            idle_timeout: self.idle_timeout,
        }
    }
}

/// One frame that is ready to be decoded and played.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub sequence: i64,
    pub data: Vec<u8>,
    /// Number of decoder PLC frames to insert before this one (0 = none).
    pub lost: u32,
    /// The decoder state must be reset before this frame (gap too large or
    /// "speaker stopped").
    pub reset: bool,
}

impl Delivery {
    pub fn is_stop_frame(&self) -> bool {
        self.data.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct JitterBuffer {
    cfg: JitterConfig,
    /// Highest sequence number handed to the decoder, `None` = unset.
    last: Option<i64>,
    pending: BTreeMap<i64, Vec<u8>>,
    last_activity: Option<Instant>,
    stale_drops: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        JitterBuffer::new(JitterConfig::default())
    }
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        JitterBuffer {
            cfg: cfg.sanitised(),
            last: None,
            pending: BTreeMap::new(),
            last_activity: None,
            stale_drops: 0,
        }
    }

    pub fn config(&self) -> &JitterConfig {
        &self.cfg
    }

    /// Frames currently held back, waiting for a gap to be filled.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The last sequence number delivered to the decoder.
    pub fn last_sequence(&self) -> Option<i64> {
        self.last
    }

    pub fn stale_drops(&self) -> u64 {
        self.stale_drops
    }

    /// Forget all state (e.g. on reconnect, §6.5).
    pub fn reset(&mut self) {
        self.last = None;
        self.pending.clear();
    }

    /// True if the channel has been silent longer than `idle_timeout` (§6.6:
    /// a channel with no packets for 30 000 ms is closed).
    pub fn is_idle(&self, now: Instant) -> bool {
        match self.last_activity {
            Some(t) => now.saturating_duration_since(t) > self.cfg.idle_timeout,
            None => true,
        }
    }

    /// Feed one sound packet. Returns every frame that became playable, in
    /// playback order.
    ///
    /// `SPEC-AMBIGUITY`: for an empty payload §6.6 says "do not play anything"
    /// and reset the buffer. A `Delivery` with `reset = true` and empty `data`
    /// is still returned so the caller can reset its decoder; it must not be
    /// decoded as audio.
    pub fn push(&mut self, now: Instant, sequence: i64, data: &[u8]) -> Vec<Delivery> {
        self.last_activity = Some(now);

        // "Speaker stopped": reset decoder state and buffer, play nothing.
        if data.is_empty() {
            self.pending.clear();
            self.last = None;
            return vec![Delivery {
                sequence,
                data: Vec::new(),
                lost: 0,
                reset: true,
            }];
        }

        if let Some(last) = self.last {
            if sequence <= last {
                self.stale_drops += 1;
                return Vec::new();
            }
        }
        if self.pending.contains_key(&sequence) {
            return Vec::new();
        }

        let in_order = match self.last {
            None => true,
            Some(last) => sequence == last + 1,
        };
        if in_order {
            self.last = Some(sequence);
            let mut out = vec![Delivery {
                sequence,
                data: data.to_vec(),
                lost: 0,
                reset: false,
            }];
            out.append(&mut self.drain_contiguous());
            return out;
        }

        self.pending.insert(sequence, data.to_vec());
        if self.pending.len() > self.cfg.threshold {
            return self.flush_lowest();
        }
        Vec::new()
    }

    fn flush_lowest(&mut self) -> Vec<Delivery> {
        let Some(&sequence) = self.pending.keys().next() else {
            return Vec::new();
        };
        let data = self.pending.remove(&sequence).expect("key just seen");
        let (lost, reset) = match self.last {
            Some(last) => {
                let gap = sequence.saturating_sub(last.saturating_add(1));
                if gap < 0 {
                    (0, true)
                } else if gap as u64 > self.cfg.max_gap_fill as u64 {
                    // Too large to conceal (§6.6): reset the decoder instead.
                    (0, true)
                } else {
                    (gap as u32, false)
                }
            }
            None => (0, false),
        };
        self.last = Some(sequence);
        let mut out = vec![Delivery {
            sequence,
            data,
            lost,
            reset,
        }];
        out.append(&mut self.drain_contiguous());
        out
    }

    fn drain_contiguous(&mut self) -> Vec<Delivery> {
        let mut out = Vec::new();
        loop {
            let next = match self.last {
                Some(last) => last.saturating_add(1),
                None => break,
            };
            match self.pending.remove(&next) {
                Some(data) => {
                    self.last = Some(next);
                    out.push(Delivery {
                        sequence: next,
                        data,
                        lost: 0,
                        reset: false,
                    });
                }
                None => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn cfg(threshold: usize) -> JitterConfig {
        JitterConfig {
            capacity: 16,
            threshold,
            max_gap_fill: 5,
            idle_timeout: ms(30_000),
        }
    }

    fn seqs(out: &[Delivery]) -> Vec<(i64, u32, bool)> {
        out.iter().map(|d| (d.sequence, d.lost, d.reset)).collect()
    }

    #[test]
    fn first_frame_is_delivered_immediately() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        let out = j.push(now, 0, &[1, 2, 3]);
        assert_eq!(seqs(&out), vec![(0, 0, false)]);
        assert_eq!(out[0].data, vec![1, 2, 3]);
        assert_eq!(j.pending(), 0);
    }

    #[test]
    fn in_order_frames_have_no_gap_and_no_buffering() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        for i in 0..10i64 {
            let out = j.push(now, i, &[i as u8]);
            assert_eq!(seqs(&out), vec![(i, 0, false)]);
        }
        assert_eq!(j.pending(), 0);
    }

    #[test]
    fn small_reorder_is_repaired_by_the_expected_packet() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        assert_eq!(j.push(now, 0, &[0]).len(), 1);
        // 2 arrives before 1 -> buffered, nothing to play yet.
        assert!(j.push(now, 2, &[2]).is_empty());
        assert_eq!(j.pending(), 1);
        // 1 fills the gap: 1 plays now, 2 follows immediately.
        assert_eq!(seqs(&j.push(now, 1, &[1])), vec![(1, 0, false), (2, 0, false)]);
        assert_eq!(j.pending(), 0);
        assert_eq!(j.last_sequence(), Some(2));
    }

    #[test]
    fn stale_and_duplicate_frames_are_dropped() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        j.push(now, 5, &[5]);
        assert!(j.push(now, 5, &[5]).is_empty());
        assert!(j.push(now, 4, &[4]).is_empty());
        assert!(j.push(now, 0, &[0]).is_empty());
        assert_eq!(j.stale_drops(), 3);
        assert_eq!(j.last_sequence(), Some(5));
    }

    #[test]
    fn buffered_duplicate_is_dropped() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        j.push(now, 0, &[0]);
        j.push(now, 3, &[3]);
        j.push(now, 3, &[3]);
        assert_eq!(j.pending(), 1);
    }

    #[test]
    fn threshold_flush_reports_the_gap_as_lost() {
        let mut j = JitterBuffer::new(cfg(3));
        let now = Instant::now();
        j.push(now, 0, &[0]); // played
        j.push(now, 2, &[2]); // buffered
        j.push(now, 3, &[3]);
        j.push(now, 4, &[4]); // 3 buffered == threshold, still waiting
        assert_eq!(j.pending(), 3);
        // 7 pushes the buffer over the threshold -> flush from 2.
        let out = j.push(now, 7, &[7]);
        assert_eq!(
            seqs(&out),
            vec![(2, 1, false), (3, 0, false), (4, 0, false)]
        );
        assert_eq!(j.pending(), 1, "7 stays buffered until 5/6 arrive");
        assert_eq!(j.last_sequence(), Some(4));
    }

    #[test]
    fn gap_larger_than_plc_limit_resets_instead_of_concealing() {
        let mut j = JitterBuffer::new(cfg(0)); // threshold 0 = no reordering
        let now = Instant::now();
        j.push(now, 0, &[0]);
        let out = j.push(now, 10, &[10]);
        assert_eq!(seqs(&out), vec![(10, 0, true)]);
        assert!(out[0].reset);
    }

    #[test]
    fn gap_equal_to_the_limit_is_concealed() {
        // last = 0, next = 6 -> gap 5 == max_gap_fill -> concealed, no reset.
        let mut j = JitterBuffer::new(cfg(0));
        let now = Instant::now();
        j.push(now, 0, &[0]);
        let out = j.push(now, 6, &[6]);
        assert_eq!(seqs(&out), vec![(6, 5, false)]);
    }

    #[test]
    fn stop_frame_resets_everything_and_plays_nothing() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        j.push(now, 0, &[0]);
        j.push(now, 3, &[3]);
        assert_eq!(j.pending(), 1);
        let out = j.push(now, 4, &[]);
        assert_eq!(seqs(&out), vec![(4, 0, true)]);
        assert!(out[0].is_stop_frame());
        assert_eq!(j.pending(), 0);
        assert_eq!(j.last_sequence(), None, "§6.6: lastSequenceNumber is unset");
        // The speaker may restart at 0 without being treated as stale.
        assert_eq!(seqs(&j.push(now, 0, &[9])), vec![(0, 0, false)]);
    }

    #[test]
    fn reconnect_sequence_jump_backwards_is_stale_until_reset() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        for i in 0..5i64 {
            j.push(now, i, &[i as u8]);
        }
        // §6.5: after a reconnect the speaker restarts at 0.
        assert!(j.push(now, 0, &[0]).is_empty());
        j.reset();
        assert_eq!(seqs(&j.push(now, 0, &[0])), vec![(0, 0, false)]);
    }

    #[test]
    fn capacity_is_a_hard_bound() {
        let mut j = JitterBuffer::new(JitterConfig {
            capacity: 4,
            threshold: 3,
            max_gap_fill: 5,
            idle_timeout: ms(1000),
        });
        let now = Instant::now();
        j.push(now, 0, &[0]);
        for seq in 100..120i64 {
            j.push(now, seq, &[1]);
            assert!(j.pending() <= 4, "buffer grew past capacity: {}", j.pending());
        }
    }

    #[test]
    fn idle_timeout_closes_the_channel() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        assert!(j.is_idle(now), "a never-used channel is idle");
        j.push(now, 0, &[0]);
        assert!(!j.is_idle(now + ms(29_999)));
        assert!(j.is_idle(now + ms(30_001)));
    }

    #[test]
    fn out_of_order_flush_preserves_playback_order() {
        // threshold 1: a single held-back frame is enough to force a flush.
        let mut j = JitterBuffer::new(cfg(1));
        let now = Instant::now();
        j.push(now, 0, &[0]);
        // 2 arrives first and is held; 1 then fills the gap and drags 2 along.
        let mut played = Vec::new();
        played.extend(j.push(now, 2, &[2]).into_iter().map(|d| d.sequence));
        played.extend(j.push(now, 1, &[1]).into_iter().map(|d| d.sequence));
        // Same for 4 before 3.
        played.extend(j.push(now, 4, &[4]).into_iter().map(|d| d.sequence));
        played.extend(j.push(now, 3, &[3]).into_iter().map(|d| d.sequence));
        assert_eq!(played, vec![1, 2, 3, 4]);
    }

    #[test]
    fn saturating_arithmetic_survives_extreme_sequence_numbers() {
        let mut j = JitterBuffer::default();
        let now = Instant::now();
        j.push(now, i64::MAX - 1, &[1]);
        assert!(j.push(now, i64::MAX, &[2]).len() == 1);
        // No overflow when computing last + 1 for the next expected frame.
        assert!(j.push(now, i64::MIN, &[3]).is_empty());
    }

    #[test]
    fn config_is_sanitised() {
        let j = JitterBuffer::new(JitterConfig {
            capacity: 0,
            threshold: 999,
            max_gap_fill: 5,
            idle_timeout: ms(1),
        });
        assert_eq!(j.config().capacity, 1);
        assert_eq!(j.config().threshold, 1);
    }
}
