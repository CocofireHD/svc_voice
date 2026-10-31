//! Primitive encodings shared by every packet (SPEC §0.1).
//!
//! Every fixed-width integer in SVC is **big-endian** (§0.2); the only
//! variable-length encodings are the Minecraft `VarInt` and the structures
//! built on it (`String`, `byte[]`).
//!
//! Decoding never panics on truncated input: every read is bounds checked and
//! returns [`WireError::UnexpectedEof`] instead of slicing out of range.

use std::fmt;

/// A Minecraft VarInt never occupies more than five bytes (§0.1).
pub const VARINT_MAX_BYTES: usize = 5;
/// Length of a `UUID` on the wire: two big-endian `long`s, no length prefix.
pub const UUID_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Not enough bytes left for the requested read.
    UnexpectedEof { needed: usize, remaining: usize },
    /// A VarInt that continued past its fifth byte.
    VarIntTooLong,
    /// A length prefix was negative.
    NegativeLength(i32),
    /// A `String` had more characters than the field allows (encoder side).
    StringCharLimit { len: usize, max: usize },
    /// A `String` had more bytes than `max_chars * 3` (both sides).
    StringByteLimit { bytes: usize, max: usize },
    /// A `String` payload was not valid UTF-8.
    InvalidUtf8,
    /// A `byte[]` exceeded the field limit.
    ByteArrayLimit { len: usize, max: usize },
    /// A packet decoder was handed more bytes than the structure consumes.
    TrailingBytes { count: usize },
    /// A textual UUID could not be parsed.
    InvalidUuid,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::UnexpectedEof { needed, remaining } => {
                write!(f, "unexpected end of buffer: need {needed} bytes, {remaining} left")
            }
            WireError::VarIntTooLong => write!(f, "varint exceeds {VARINT_MAX_BYTES} bytes"),
            WireError::NegativeLength(v) => write!(f, "negative length prefix {v}"),
            WireError::StringCharLimit { len, max } => {
                write!(f, "string has {len} chars, limit is {max}")
            }
            WireError::StringByteLimit { bytes, max } => {
                write!(f, "string has {bytes} bytes, limit is {max}")
            }
            WireError::InvalidUtf8 => write!(f, "string is not valid utf-8"),
            WireError::ByteArrayLimit { len, max } => {
                write!(f, "byte array has {len} bytes, limit is {max}")
            }
            WireError::TrailingBytes { count } => write!(f, "{count} trailing bytes after packet"),
            WireError::InvalidUuid => write!(f, "invalid uuid literal"),
        }
    }
}

impl std::error::Error for WireError {}

// ---------------------------------------------------------------------------
// UUID
// ---------------------------------------------------------------------------

/// A 128-bit UUID, stored as the two big-endian `long`s used on the wire.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Uuid {
    pub msb: i64,
    pub lsb: i64,
}

impl Uuid {
    pub const NIL: Uuid = Uuid { msb: 0, lsb: 0 };

    pub const fn from_parts(msb: i64, lsb: i64) -> Self {
        Uuid { msb, lsb }
    }

    pub fn from_bytes(bytes: [u8; UUID_BYTES]) -> Self {
        let mut msb = [0u8; 8];
        let mut lsb = [0u8; 8];
        msb.copy_from_slice(&bytes[..8]);
        lsb.copy_from_slice(&bytes[8..]);
        Uuid {
            msb: i64::from_be_bytes(msb),
            lsb: i64::from_be_bytes(lsb),
        }
    }

    pub fn to_bytes(self) -> [u8; UUID_BYTES] {
        let mut out = [0u8; UUID_BYTES];
        out[..8].copy_from_slice(&self.msb.to_be_bytes());
        out[8..].copy_from_slice(&self.lsb.to_be_bytes());
        out
    }

    pub const fn from_u128(v: u128) -> Self {
        Uuid {
            msb: (v >> 64) as i64,
            lsb: v as i64,
        }
    }

    pub const fn as_u128(self) -> u128 {
        ((self.msb as u64 as u128) << 64) | (self.lsb as u64 as u128)
    }

    /// Parse the canonical `8-4-4-4-12` form (and the unhyphenated 32-char form).
    pub fn parse_str(s: &str) -> Result<Self, WireError> {
        let mut nibbles = [0u8; 32];
        let mut n = 0usize;
        for (i, b) in s.bytes().enumerate() {
            if b == b'-' {
                // Hyphens are only allowed at the canonical positions.
                if !matches!(i, 8 | 13 | 18 | 23) {
                    return Err(WireError::InvalidUuid);
                }
                continue;
            }
            let v = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(WireError::InvalidUuid),
            };
            if n == 32 {
                return Err(WireError::InvalidUuid);
            }
            nibbles[n] = v;
            n += 1;
        }
        if n != 32 {
            return Err(WireError::InvalidUuid);
        }
        let mut bytes = [0u8; UUID_BYTES];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (nibbles[i * 2] << 4) | nibbles[i * 2 + 1];
        }
        Ok(Uuid::from_bytes(bytes))
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.to_bytes();
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Bounds-checked cursor over a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::UnexpectedEof {
                needed: n,
                remaining: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn read_u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    pub fn read_i8(&mut self) -> Result<i8, WireError> {
        Ok(self.read_u8()? as i8)
    }

    pub fn read_bool(&mut self) -> Result<bool, WireError> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_i16(&mut self) -> Result<i16, WireError> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_i32(&mut self) -> Result<i32, WireError> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_i64(&mut self) -> Result<i64, WireError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(i64::from_be_bytes(a))
    }

    pub fn read_f32(&mut self) -> Result<f32, WireError> {
        Ok(f32::from_bits(self.read_i32()? as u32))
    }

    pub fn read_f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_bits(self.read_i64()? as u64))
    }

    pub fn read_uuid(&mut self) -> Result<Uuid, WireError> {
        let b = self.take(UUID_BYTES)?;
        let mut a = [0u8; UUID_BYTES];
        a.copy_from_slice(b);
        Ok(Uuid::from_bytes(a))
    }

    /// Read a fixed 16-byte secret (no length prefix, §2.3 / §5.5).
    pub fn read_fixed_secret(&mut self) -> Result<[u8; 16], WireError> {
        let b = self.take(16)?;
        let mut a = [0u8; 16];
        a.copy_from_slice(b);
        Ok(a)
    }

    /// Minecraft VarInt: 7 data bits per byte, LSB group first, `0x80` marks
    /// "more bytes follow", at most five bytes for a 32-bit value (§0.1).
    pub fn read_varint(&mut self) -> Result<i32, WireError> {
        let mut value: u32 = 0;
        for i in 0..VARINT_MAX_BYTES {
            let byte = self.read_u8()?;
            let shift = (i * 7) as u32;
            value |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        Err(WireError::VarIntTooLong)
    }

    fn read_bytes_prefixed(&mut self, max: usize) -> Result<&'a [u8], WireError> {
        let len = self.read_varint()?;
        if len < 0 {
            return Err(WireError::NegativeLength(len));
        }
        let len = len as usize;
        if len > max {
            return Err(WireError::ByteArrayLimit { len, max });
        }
        self.take(len)
    }

    /// `byte[]`: VarInt length + raw bytes, hard-capped at `max` bytes (§0.1).
    pub fn read_byte_array(&mut self, max: usize) -> Result<&'a [u8], WireError> {
        self.read_bytes_prefixed(max)
    }

    /// `String`: VarInt byte-length + UTF-8, with the Java `max*3` byte bound
    /// and the character limit applied on the encoder side only (§0.1).
    ///
    /// `SPEC-AMBIGUITY`: §0.1 says the decoder rejects byte-lengths above
    /// `max*3`; it does not say the decoder counts characters. It does not, so
    /// a decoder and the reference reader accept the same payloads
    /// (OPEN_QUESTIONS #13).
    pub fn read_string(&mut self, max_chars: usize) -> Result<String, WireError> {
        let len = self.read_varint()?;
        if len < 0 {
            return Err(WireError::NegativeLength(len));
        }
        let len = len as usize;
        let byte_cap = max_chars.saturating_mul(3);
        if len > byte_cap {
            return Err(WireError::StringByteLimit {
                bytes: len,
                max: byte_cap,
            });
        }
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| WireError::InvalidUtf8)
    }

    /// Error if the buffer still holds bytes the structure did not consume.
    pub fn ensure_end(&self) -> Result<(), WireError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(WireError::TrailingBytes {
                count: self.remaining(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Growable output buffer with the same primitives as [`Reader`].
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Writer {
            buf: Vec::with_capacity(n),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(if v { 1 } else { 0 });
    }

    pub fn write_i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    pub fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    pub fn write_uuid(&mut self, v: Uuid) {
        self.buf.extend_from_slice(&v.to_bytes());
    }

    pub fn write_fixed_secret(&mut self, v: &[u8; 16]) {
        self.buf.extend_from_slice(v);
    }

    pub fn write_varint(&mut self, value: i32) {
        let mut v = value as u32;
        loop {
            if v & !0x7F == 0 {
                self.buf.push(v as u8);
                return;
            }
            self.buf.push(((v & 0x7F) | 0x80) as u8);
            v >>= 7;
        }
    }

    /// `byte[]` with a hard size limit; the limit is enforced before writing.
    pub fn write_byte_array(&mut self, bytes: &[u8], max: usize) -> Result<(), WireError> {
        if bytes.len() > max {
            return Err(WireError::ByteArrayLimit {
                len: bytes.len(),
                max,
            });
        }
        self.write_varint(bytes.len() as i32);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// `String` with the Java `writeUtf` semantics: character limit first, then
    /// the `max*3` byte sanity bound (parity with the decoder).
    pub fn write_string(&mut self, s: &str, max_chars: usize) -> Result<(), WireError> {
        let chars = s.chars().count();
        if chars > max_chars {
            return Err(WireError::StringCharLimit {
                len: chars,
                max: max_chars,
            });
        }
        let bytes = s.as_bytes();
        let byte_cap = max_chars.saturating_mul(3);
        if bytes.len() > byte_cap {
            return Err(WireError::StringByteLimit {
                bytes: bytes.len(),
                max: byte_cap,
            });
        }
        self.write_varint(bytes.len() as i32);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
}

/// Encode a single VarInt in isolation (handy for tests and framing).
pub fn varint_bytes(value: i32) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_varint(value);
    w.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_varint(v: i32) -> Vec<u8> {
        varint_bytes(v)
    }

    #[test]
    fn varint_zero_is_one_byte() {
        assert_eq!(rt_varint(0), vec![0x00]);
    }

    #[test]
    fn varint_single_byte_boundaries() {
        assert_eq!(rt_varint(1), vec![0x01]);
        assert_eq!(rt_varint(127), vec![0x7F]);
    }

    #[test]
    fn varint_two_bytes_at_128() {
        assert_eq!(rt_varint(128), vec![0x80, 0x01]);
        assert_eq!(rt_varint(255), vec![0xFF, 0x01]);
    }

    #[test]
    fn varint_known_vectors() {
        // 300 = 0b1_0010_1100 -> groups 0101100 (0x2C|0x80), 0000010
        assert_eq!(rt_varint(300), vec![0xAC, 0x02]);
        // 24454 -> classic port example: 24454 = 0b101_1111_1000_0110
        assert_eq!(rt_varint(24454), vec![0x86, 0xBF, 0x01]);
        // 2097151 is the largest 3-byte value
        assert_eq!(rt_varint(2_097_151), vec![0xFF, 0xFF, 0x7F]);
    }

    #[test]
    fn varint_max_i32_is_five_bytes() {
        let bytes = rt_varint(i32::MAX);
        assert_eq!(bytes.len(), 5);
        assert_eq!(bytes, vec![0xFF, 0xFF, 0xFF, 0xFF, 0x07]);
    }

    #[test]
    fn varint_negative_takes_five_bytes() {
        assert_eq!(rt_varint(-1), vec![0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
        assert_eq!(rt_varint(i32::MIN), vec![0x80, 0x80, 0x80, 0x80, 0x08]);
    }

    #[test]
    fn varint_roundtrip_all_interesting_values() {
        for v in [
            0,
            1,
            2,
            63,
            64,
            127,
            128,
            300,
            16_383,
            16_384,
            1_048_575,
            2_097_151,
            i32::MAX,
            -1,
            -128,
            -2_097_152,
            i32::MIN,
        ] {
            let bytes = rt_varint(v);
            let mut r = Reader::new(&bytes);
            assert_eq!(r.read_varint().unwrap(), v, "value {v}");
            assert_eq!(r.remaining(), 0);
        }
    }

    #[test]
    fn varint_rejects_six_bytes() {
        let bytes = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x00];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_varint(), Err(WireError::VarIntTooLong));
    }

    #[test]
    fn varint_rejects_five_continuation_bytes() {
        // Four continuation bytes are legal, the fifth byte must not set 0x80.
        let bytes = [0xFFu8, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_varint(), Err(WireError::VarIntTooLong));
    }

    #[test]
    fn varint_truncated_is_an_error_not_a_panic() {
        let mut r = Reader::new(&[0x80, 0x01][..1]);
        assert_eq!(
            r.read_varint(),
            Err(WireError::UnexpectedEof {
                needed: 1,
                remaining: 0
            })
        );
    }

    #[test]
    fn string_roundtrip_ascii_and_multibyte() {
        let mut w = Writer::new();
        w.write_string("hello", 32).unwrap();
        w.write_string("grüße", 32).unwrap();
        w.write_string("", 32).unwrap();
        let bytes = w.into_inner();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_string(32).unwrap(), "hello");
        assert_eq!(r.read_string(32).unwrap(), "grüße");
        assert_eq!(r.read_string(32).unwrap(), "");
        assert!(r.is_empty());
    }

    #[test]
    fn string_length_prefix_is_byte_length_not_char_length() {
        let mut w = Writer::new();
        w.write_string("ä", 16).unwrap();
        assert_eq!(w.as_slice(), &[0x02, 0xC3, 0xA4]);
    }

    #[test]
    fn string_encoder_enforces_char_limit() {
        let mut w = Writer::new();
        assert_eq!(
            w.write_string("abcdef", 5),
            Err(WireError::StringCharLimit { len: 6, max: 5 })
        );
    }

    #[test]
    fn string_encoder_enforces_three_byte_per_char_cap() {
        // 4 chars, each 4 bytes in UTF-8 = 16 bytes > 4*3.
        let mut w = Writer::new();
        assert_eq!(
            w.write_string("😀😀😀😀", 4),
            Err(WireError::StringByteLimit { bytes: 16, max: 12 })
        );
    }

    #[test]
    fn string_decoder_rejects_overlong_byte_length() {
        // Length 7 with max_chars 2 -> 2*3 = 6 allowed.
        let bytes = [0x07u8, b'a', b'b', b'c', b'd', b'e', b'f', b'g'];
        let mut r = Reader::new(&bytes);
        assert_eq!(
            r.read_string(2),
            Err(WireError::StringByteLimit { bytes: 7, max: 6 })
        );
    }

    #[test]
    fn string_decoder_rejects_invalid_utf8() {
        let bytes = [0x02u8, 0xC3, 0x28];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_string(16), Err(WireError::InvalidUtf8));
    }

    #[test]
    fn string_decoder_rejects_negative_length() {
        let bytes = varint_bytes(-1);
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_string(16), Err(WireError::NegativeLength(-1)));
    }

    #[test]
    fn uuid_roundtrip_preserves_bit_pattern() {
        let raw = [
            0x58u8, 0xbc, 0x9a, 0xe9, 0xc7, 0xa8, 0x45, 0xe4, 0xa1, 0x1c, 0xef, 0xbb, 0x67, 0x19,
            0x94, 0x25,
        ];
        let uuid = Uuid::from_bytes(raw);
        assert_eq!(uuid.to_bytes(), raw);
        assert_eq!(uuid.to_string(), "58bc9ae9-c7a8-45e4-a11c-efbb67199425");
        assert_eq!(Uuid::parse_str("58bc9ae9-c7a8-45e4-a11c-efbb67199425"), Ok(uuid));
    }

    #[test]
    fn uuid_wire_is_msb_then_lsb_big_endian() {
        let uuid = Uuid::from_parts(0x0102_0304_0506_0708, 0x1112_1314_1516_1718);
        let mut w = Writer::new();
        w.write_uuid(uuid);
        assert_eq!(
            w.as_slice(),
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18
            ]
        );
    }

    #[test]
    fn uuid_parse_rejects_malformed() {
        assert_eq!(Uuid::parse_str("nope"), Err(WireError::InvalidUuid));
        assert_eq!(
            Uuid::parse_str("58bc-9ae9-c7a8-45e4-a11c-efbb67199425"),
            Err(WireError::InvalidUuid)
        );
        assert_eq!(Uuid::parse_str(""), Err(WireError::InvalidUuid));
    }

    #[test]
    fn uuid_u128_roundtrip() {
        let v = 0x1234_5678_9abc_def0_0fed_cba9_8765_4321u128;
        assert_eq!(Uuid::from_u128(v).as_u128(), v);
    }

    #[test]
    fn byte_array_roundtrip_and_limit() {
        let mut w = Writer::new();
        w.write_byte_array(&[1, 2, 3], 8).unwrap();
        w.write_byte_array(&[], 8).unwrap();
        let bytes = w.into_inner();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_byte_array(8).unwrap(), &[1, 2, 3]);
        assert_eq!(r.read_byte_array(8).unwrap(), &[0u8; 0]);
        assert!(r.is_empty());

        let mut w2 = Writer::new();
        assert_eq!(
            w2.write_byte_array(&[0u8; 9], 8),
            Err(WireError::ByteArrayLimit { len: 9, max: 8 })
        );
    }

    #[test]
    fn byte_array_decoder_rejects_oversized_declaration() {
        // Declares 9 bytes but only 3 follow; the limit triggers first.
        let bytes = [0x09u8, 1, 2, 3];
        let mut r = Reader::new(&bytes);
        assert_eq!(
            r.read_byte_array(8),
            Err(WireError::ByteArrayLimit { len: 9, max: 8 })
        );
    }

    #[test]
    fn fixed_width_roundtrip() {
        let mut w = Writer::new();
        w.write_i8(-5);
        w.write_bool(true);
        w.write_bool(false);
        w.write_i16(-1234);
        w.write_i32(-123_456);
        w.write_i64(i64::MIN + 7);
        w.write_f32(1.5);
        w.write_f64(-48.25);
        let bytes = w.into_inner();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_i8().unwrap(), -5);
        assert!(r.read_bool().unwrap());
        assert!(!r.read_bool().unwrap());
        assert_eq!(r.read_i16().unwrap(), -1234);
        assert_eq!(r.read_i32().unwrap(), -123_456);
        assert_eq!(r.read_i64().unwrap(), i64::MIN + 7);
        assert_eq!(r.read_f32().unwrap(), 1.5);
        assert_eq!(r.read_f64().unwrap(), -48.25);
        assert!(r.is_empty());
    }

    #[test]
    fn integers_are_big_endian() {
        let mut w = Writer::new();
        w.write_i32(1);
        assert_eq!(w.as_slice(), &[0, 0, 0, 1]);
        let mut w = Writer::new();
        w.write_i16(1);
        assert_eq!(w.as_slice(), &[0, 1]);
        let mut w = Writer::new();
        w.write_i64(1);
        assert_eq!(w.as_slice(), &[0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn truncated_reads_report_shortfall() {
        let bytes = [0u8, 1, 2];
        let mut r = Reader::new(&bytes);
        assert_eq!(
            r.read_i32(),
            Err(WireError::UnexpectedEof {
                needed: 4,
                remaining: 3
            })
        );
        // The cursor must not have advanced on failure.
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn empty_buffer_every_primitive_is_an_error() {
        macro_rules! must_err {
            ($($m:ident),*) => {$(
                let mut r = Reader::new(&[]);
                assert!(r.$m().is_err(), stringify!($m));
            )*};
        }
        must_err!(
            read_u8,
            read_i8,
            read_bool,
            read_i16,
            read_i32,
            read_i64,
            read_f32,
            read_f64,
            read_uuid,
            read_varint
        );
    }

    #[test]
    fn ensure_end_detects_trailing_bytes() {
        let mut r = Reader::new(&[1, 2, 3]);
        r.read_u8().unwrap();
        assert_eq!(r.ensure_end(), Err(WireError::TrailingBytes { count: 2 }));
    }
}
