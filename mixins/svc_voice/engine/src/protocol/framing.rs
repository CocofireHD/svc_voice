//! UDP datagram framing (SPEC §3).
//!
//! Client → server: `0xFF ‖ UUID(16) ‖ VarInt(len) ‖ blob`
//! Server → client: `0xFF ‖ VarInt(len) ‖ blob` — **no UUID** in this
//! direction (§3.2); expecting one is the classic parse-everything bug.
//!
//! The packet id is not visible here: it lives inside the encrypted blob.

use std::fmt;

use crate::protocol::wire::{Reader, Uuid, WireError};

/// Protocol magic byte (§0.3).
pub const MAGIC: u8 = 0xFF;
/// Maximum encrypted blob accepted in either direction (§3.3).
pub const MAX_BLOB_LEN: usize = 2048;
/// UDP receive buffer: a datagram of this size or larger is rejected (§3.3).
pub const MAX_DATAGRAM_LEN: usize = 4096;
/// Worst case a client has to send: magic + UUID + 3-byte length + 2048 (§3.3).
pub const MAX_CLIENT_DATAGRAM_LEN: usize = 1 + 16 + 3 + MAX_BLOB_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Empty datagram.
    Empty,
    /// First byte was not `0xFF`.
    BadMagic(u8),
    /// Datagram length is at or above [`MAX_DATAGRAM_LEN`].
    DatagramTooLarge { len: usize },
    /// Declared blob length exceeds [`MAX_BLOB_LEN`].
    BlobTooLarge { len: usize },
    /// Not enough bytes for the header or the declared blob.
    Truncated { needed: usize, remaining: usize },
    /// Bytes after the declared blob (strict parsing, see README).
    TrailingBytes { count: usize },
    /// Malformed length prefix.
    Wire(WireError),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Empty => write!(f, "empty datagram"),
            FrameError::BadMagic(b) => write!(f, "bad magic byte 0x{b:02x}"),
            FrameError::DatagramTooLarge { len } => {
                write!(f, "datagram of {len} bytes exceeds the {MAX_DATAGRAM_LEN}-byte limit")
            }
            FrameError::BlobTooLarge { len } => {
                write!(f, "blob of {len} bytes exceeds the {MAX_BLOB_LEN}-byte limit")
            }
            FrameError::Truncated { needed, remaining } => {
                write!(f, "truncated datagram: need {needed} bytes, {remaining} left")
            }
            FrameError::TrailingBytes { count } => write!(f, "{count} trailing bytes after blob"),
            FrameError::Wire(e) => write!(f, "length prefix: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<WireError> for FrameError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::UnexpectedEof { needed, remaining } => {
                FrameError::Truncated { needed, remaining }
            }
            other => FrameError::Wire(other),
        }
    }
}

fn check_size(len: usize) -> Result<(), FrameError> {
    if len >= MAX_DATAGRAM_LEN {
        return Err(FrameError::DatagramTooLarge { len });
    }
    Ok(())
}

fn check_blob(blob: &[u8]) -> Result<(), FrameError> {
    if blob.len() > MAX_BLOB_LEN {
        return Err(FrameError::BlobTooLarge { len: blob.len() });
    }
    Ok(())
}

fn require(buf: &[u8], n: usize) -> Result<(), FrameError> {
    if buf.len() < n {
        return Err(FrameError::Truncated {
            needed: n,
            remaining: buf.len(),
        });
    }
    Ok(())
}

/// Client → server datagram (§3.1).
pub fn encode_client(player: Uuid, blob: &[u8]) -> Result<Vec<u8>, FrameError> {
    check_blob(blob)?;
    let mut out = Vec::with_capacity(1 + 16 + 3 + blob.len());
    out.push(MAGIC);
    out.extend_from_slice(&player.to_bytes());
    // Length prefix is a VarInt; at most 2048 so at most two bytes.
    let mut len = blob.len() as u32;
    loop {
        if len & !0x7F == 0 {
            out.push(len as u8);
            break;
        }
        out.push(((len & 0x7F) | 0x80) as u8);
        len >>= 7;
    }
    out.extend_from_slice(blob);
    check_size(out.len())?;
    Ok(out)
}

/// Parse a client → server datagram into `(player, blob)`.
pub fn decode_client(datagram: &[u8]) -> Result<(Uuid, &[u8]), FrameError> {
    if datagram.is_empty() {
        return Err(FrameError::Empty);
    }
    check_size(datagram.len())?;
    if datagram[0] != MAGIC {
        return Err(FrameError::BadMagic(datagram[0]));
    }
    require(datagram, 1 + 16)?;
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&datagram[1..17]);
    let player = Uuid::from_bytes(uuid_bytes);
    let mut r = Reader::new(&datagram[17..]);
    let declared = r.read_varint()?;
    if declared < 0 {
        return Err(FrameError::Wire(WireError::NegativeLength(declared)));
    }
    let declared = declared as usize;
    if declared > MAX_BLOB_LEN {
        return Err(FrameError::BlobTooLarge { len: declared });
    }
    if r.remaining() < declared {
        return Err(FrameError::Truncated {
            needed: declared,
            remaining: r.remaining(),
        });
    }
    let start = 17 + r.position();
    let blob = &datagram[start..start + declared];
    let trailing = datagram.len() - (start + declared);
    if trailing != 0 {
        return Err(FrameError::TrailingBytes { count: trailing });
    }
    Ok((player, blob))
}

/// Server → client datagram (§3.2).
pub fn encode_server(blob: &[u8]) -> Result<Vec<u8>, FrameError> {
    check_blob(blob)?;
    let mut out = Vec::with_capacity(1 + 3 + blob.len());
    out.push(MAGIC);
    let mut len = blob.len() as u32;
    loop {
        if len & !0x7F == 0 {
            out.push(len as u8);
            break;
        }
        out.push(((len & 0x7F) | 0x80) as u8);
        len >>= 7;
    }
    out.extend_from_slice(blob);
    check_size(out.len())?;
    Ok(out)
}

/// Parse a server → client datagram into its blob.
///
/// `SPEC-AMBIGUITY`: bytes after the declared blob are an error here, although
/// the Java reader would simply stop after the blob. One packet per datagram
/// makes the strictness free and it catches desynchronised writes early.
pub fn decode_server(datagram: &[u8]) -> Result<&[u8], FrameError> {
    if datagram.is_empty() {
        return Err(FrameError::Empty);
    }
    check_size(datagram.len())?;
    if datagram[0] != MAGIC {
        return Err(FrameError::BadMagic(datagram[0]));
    }
    let mut r = Reader::new(&datagram[1..]);
    let declared = r.read_varint()?;
    if declared < 0 {
        return Err(FrameError::Wire(WireError::NegativeLength(declared)));
    }
    let declared = declared as usize;
    if declared > MAX_BLOB_LEN {
        return Err(FrameError::BlobTooLarge { len: declared });
    }
    if r.remaining() < declared {
        return Err(FrameError::Truncated {
            needed: declared,
            remaining: r.remaining(),
        });
    }
    let start = 1 + r.position();
    let blob = &datagram[start..start + declared];
    let trailing = datagram.len() - (start + declared);
    if trailing != 0 {
        return Err(FrameError::TrailingBytes { count: trailing });
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::parse_str("58bc9ae9-c7a8-45e4-a11c-efbb67199425").unwrap()
    }

    #[test]
    fn client_roundtrip() {
        let blob = vec![0xABu8; 40];
        let datagram = encode_client(uuid(), &blob).unwrap();
        assert_eq!(datagram[0], 0xFF);
        assert_eq!(&datagram[1..17], &uuid().to_bytes());
        assert_eq!(datagram[17], 40);
        assert_eq!(datagram.len(), 1 + 16 + 1 + 40);
        let (player, out) = decode_client(&datagram).unwrap();
        assert_eq!(player, uuid());
        assert_eq!(out, &blob[..]);
    }

    #[test]
    fn server_roundtrip_has_no_uuid() {
        let blob = vec![0x11u8; 28];
        let datagram = encode_server(&blob).unwrap();
        assert_eq!(datagram[0], 0xFF);
        assert_eq!(datagram[1], 28);
        assert_eq!(datagram.len(), 2 + 28);
        assert_eq!(decode_server(&datagram).unwrap(), &blob[..]);
    }

    #[test]
    fn client_length_prefix_is_varint_for_large_blobs() {
        let blob = vec![0u8; 300];
        let datagram = encode_client(uuid(), &blob).unwrap();
        // 300 -> 0xAC 0x02
        assert_eq!(&datagram[17..19], &[0xAC, 0x02]);
        assert_eq!(decode_client(&datagram).unwrap().1.len(), 300);
    }

    #[test]
    fn wrong_magic_is_rejected_in_both_directions() {
        let mut d = encode_server(&[1, 2, 3]).unwrap();
        d[0] = 0xFE;
        assert_eq!(decode_server(&d), Err(FrameError::BadMagic(0xFE)));
        let mut d = encode_client(uuid(), &[1, 2, 3]).unwrap();
        d[0] = 0x00;
        assert_eq!(decode_client(&d), Err(FrameError::BadMagic(0x00)));
    }

    #[test]
    fn empty_datagram_is_rejected() {
        assert_eq!(decode_client(&[]), Err(FrameError::Empty));
        assert_eq!(decode_server(&[]), Err(FrameError::Empty));
    }

    #[test]
    fn truncated_uuid_is_rejected() {
        let d = [0xFFu8, 1, 2, 3];
        assert_eq!(
            decode_client(&d),
            Err(FrameError::Truncated {
                needed: 17,
                remaining: 4
            })
        );
    }

    #[test]
    fn truncated_blob_is_rejected() {
        // Claims 40 bytes but supplies 3.
        let mut d = vec![0xFFu8];
        d.extend_from_slice(&uuid().to_bytes());
        d.push(40);
        d.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            decode_client(&d),
            Err(FrameError::Truncated {
                needed: 40,
                remaining: 3
            })
        );
    }

    #[test]
    fn oversized_declared_blob_is_rejected() {
        let mut d = vec![0xFFu8];
        d.push(0xFF);
        d.push(0x1F); // 4095
        assert_eq!(decode_server(&d), Err(FrameError::BlobTooLarge { len: 4095 }));
    }

    #[test]
    fn blob_above_limit_cannot_be_encoded() {
        assert_eq!(
            encode_server(&vec![0u8; MAX_BLOB_LEN + 1]),
            Err(FrameError::BlobTooLarge {
                len: MAX_BLOB_LEN + 1
            })
        );
        assert!(encode_server(&vec![0u8; MAX_BLOB_LEN]).is_ok());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut d = encode_server(&[1, 2, 3]).unwrap();
        d.push(0xAA);
        assert_eq!(decode_server(&d), Err(FrameError::TrailingBytes { count: 1 }));
    }

    #[test]
    fn datagram_at_hard_limit_is_rejected() {
        let mut d = vec![0u8; MAX_DATAGRAM_LEN];
        d[0] = 0xFF;
        assert_eq!(
            decode_server(&d),
            Err(FrameError::DatagramTooLarge {
                len: MAX_DATAGRAM_LEN
            })
        );
    }

    #[test]
    fn worst_case_client_datagram_fits_the_buffer() {
        let datagram = encode_client(uuid(), &vec![0u8; MAX_BLOB_LEN]).unwrap();
        assert_eq!(datagram.len(), 1 + 16 + 2 + MAX_BLOB_LEN);
        assert!(datagram.len() <= MAX_CLIENT_DATAGRAM_LEN);
        assert!(datagram.len() < MAX_DATAGRAM_LEN);
    }

    #[test]
    fn client_datagram_without_blob_is_valid() {
        // A zero-length blob cannot decrypt, but the frame itself is well formed.
        let d = encode_client(uuid(), &[]).unwrap();
        assert_eq!(d.len(), 18);
        assert_eq!(decode_client(&d).unwrap().1.len(), 0);
    }
}
