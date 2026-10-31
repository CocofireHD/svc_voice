//! Recording: one 16-bit mono WAV file per speaker.
//!
//! Files are written as they arrive so a long session does not pile up in
//! memory; the header carries placeholder sizes and is patched when the track
//! is closed, which is the one thing a plain append-only writer cannot do.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::protocol::Uuid;

const HEADER_LEN: u64 = 44;

struct Track {
    file: BufWriter<File>,
    path: PathBuf,
    samples: u32,
}

/// Every open track, keyed by the speaker's UUID.
pub struct Recorder {
    dir: PathBuf,
    tracks: HashMap<Uuid, Track>,
    /// Frames per second, written into each header.
    rate: u32,
}

impl Recorder {
    /// Create the directory if needed; recordings land in it, one file per
    /// speaker, named by UUID.
    pub fn open(dir: impl AsRef<Path>, rate: u32) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            tracks: HashMap::new(),
            rate,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many speakers have been recorded so far.
    pub fn tracks(&self) -> usize {
        self.tracks.len()
    }

    /// Append one decoded frame for `sender`, creating its file on first use.
    pub fn write(&mut self, sender: Uuid, pcm: &[f32]) -> std::io::Result<()> {
        if !self.tracks.contains_key(&sender) {
            let path = self.dir.join(format!("{}.wav", uuid_name(sender)));
            let file = File::create(&path)?;
            let mut w = BufWriter::new(file);
            w.write_all(&header(self.rate, 0))?;
            self.tracks.insert(
                sender,
                Track {
                    file: w,
                    path,
                    samples: 0,
                },
            );
        }
        let track = self.tracks.get_mut(&sender).expect("just inserted");
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        track.file.write_all(&bytes)?;
        track.samples += pcm.len() as u32;
        // Patch the sizes now rather than at close: a client that exits hard
        // never runs its destructors, and a WAV with a zero-length data chunk
        // plays as silence in most players.
        let data_len = track.samples.saturating_mul(2);
        let end = track.file.stream_position()?;
        track.file.seek(SeekFrom::Start(4))?;
        track.file.write_all(&(36 + data_len).to_le_bytes())?;
        track.file.seek(SeekFrom::Start(40))?;
        track.file.write_all(&data_len.to_le_bytes())?;
        track.file.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Patch every header with its real size. Called on stop and on drop.
    pub fn finish(&mut self) -> std::io::Result<()> {
        for (_, track) in self.tracks.drain() {
            let Track {
                mut file,
                path,
                samples,
            } = track;
            file.flush()?;
            let mut raw = file.into_inner().map_err(|e| e.into_error())?;
            raw.seek(SeekFrom::Start(0))?;
            raw.write_all(&header(self.rate, samples))?;
            raw.flush()?;
            eprintln!(
                "[svc-voice] recorded {} ({:.1}s) to {}",
                uuid_name_str(&path),
                samples as f32 / self.rate as f32,
                path.display()
            );
        }
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn uuid_name(id: Uuid) -> String {
    let [a, b] = [id.msb, id.lsb];
    format!("{:016x}{:016x}", a as u64, b as u64)
}

fn uuid_name_str(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// A canonical 44-byte PCM WAV header for `samples` mono 16-bit samples.
fn header(rate: u32, samples: u32) -> [u8; 44] {
    let data_len = samples.saturating_mul(2);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("svc-rec-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_header_is_a_canonical_mono_wav() {
        let h = header(48_000, 960);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 1920);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 48_000);
    }

    #[test]
    fn a_frame_becomes_two_bytes_and_the_header_is_patched() {
        let dir = scratch("write");
        let id = Uuid::from_parts(1, 2);
        {
            let mut rec = Recorder::open(&dir, 48_000).unwrap();
            rec.write(id, &[0.5, -0.5, 0.0]).unwrap();
            rec.write(id, &[0.25]).unwrap();
            assert_eq!(rec.tracks(), 1);
        }
        let path = dir.join(format!("{}.wav", uuid_name(id)));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, HEADER_LEN + 4 * 2, "four samples");
        assert_eq!(&bytes[0..4], b"RIFF");
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data_len, 8, "the header was patched, not left at zero");
        let first = i16::from_le_bytes(bytes[44..46].try_into().unwrap());
        assert!(first > 16_000, "0.5 survived as a sample: {first}");
    }

    #[test]
    fn each_speaker_gets_a_file_of_their_own() {
        let dir = scratch("two");
        let mut rec = Recorder::open(&dir, 48_000).unwrap();
        rec.write(Uuid::from_parts(1, 1), &[0.1]).unwrap();
        rec.write(Uuid::from_parts(2, 2), &[0.2]).unwrap();
        assert_eq!(rec.tracks(), 2);
        rec.finish().unwrap();
        let files = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(files, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
