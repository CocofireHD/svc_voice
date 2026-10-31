//! AES-128-GCM as used by the SVC UDP channel (SPEC §4).
//!
//! * key: the 16 raw secret bytes, used verbatim (`AES`, no KDF) — §4.1
//! * nonce: 12 random bytes per packet, CSPRNG, no counter — §4.2
//! * tag: 128 bit, appended to the ciphertext — §4.2
//! * no AAD, no padding (ciphertext length == plaintext length) — §4.2
//!
//! On the wire the blob is `IV(12) ‖ ciphertext(N) ‖ tag(16)`, i.e. `N+28`
//! bytes; the packet id lives *inside* the ciphertext (§4.3).

use std::fmt;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
use ring::rand::{SecureRandom, SystemRandom};

pub const SECRET_LEN: usize = 16;
pub const IV_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Fixed overhead of an encrypted blob: IV plus GCM tag.
pub const OVERHEAD: usize = IV_LEN + TAG_LEN;
/// Maximum encrypted blob, both directions (§3.3).
pub const MAX_BLOB_LEN: usize = 2048;
/// Maximum plaintext that still fits into an encrypted blob: 2048 − 12 − 16.
pub const MAX_PLAINTEXT_LEN: usize = MAX_BLOB_LEN - OVERHEAD; // 2020

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    BadSecretLength { len: usize },
    BlobTooShort { len: usize },
    BlobTooLarge { len: usize },
    PlaintextTooLarge { len: usize },
    /// Tag verification failed: wrong key, tampered ciphertext, tampered tag,
    /// or a wrong IV. The protocol has no way to distinguish these (§4.3).
    InvalidTag,
    /// The OS random source failed.
    RandomSource,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::BadSecretLength { len } => {
                write!(f, "secret must be {SECRET_LEN} bytes, got {len}")
            }
            CryptoError::BlobTooShort { len } => {
                write!(f, "encrypted blob must be at least {OVERHEAD} bytes, got {len}")
            }
            CryptoError::BlobTooLarge { len } => {
                write!(f, "encrypted blob exceeds {MAX_BLOB_LEN} bytes, got {len}")
            }
            CryptoError::PlaintextTooLarge { len } => {
                write!(f, "plaintext exceeds {MAX_PLAINTEXT_LEN} bytes, got {len}")
            }
            CryptoError::InvalidTag => write!(f, "aead tag verification failed"),
            CryptoError::RandomSource => write!(f, "random source unavailable"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// The 16-byte per-player key. `Debug` is redacted on purpose.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret([u8; SECRET_LEN]);

impl Secret {
    pub const fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Secret(bytes)
    }

    /// All-zero secret, only meaningful for tests and for "no secret yet".
    pub const ZERO: Secret = Secret([0u8; SECRET_LEN]);

    pub fn from_slice(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != SECRET_LEN {
            return Err(CryptoError::BadSecretLength { len: bytes.len() });
        }
        let mut out = [0u8; SECRET_LEN];
        out.copy_from_slice(bytes);
        Ok(Secret(out))
    }

    pub fn as_bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    /// Generate a fresh secret with the OS CSPRNG (server-side behaviour, §4.1).
    pub fn generate() -> Result<Self, CryptoError> {
        let mut bytes = [0u8; SECRET_LEN];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| CryptoError::RandomSource)?;
        Ok(Secret(bytes))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(**redacted**)")
    }
}

fn key(secret: &Secret) -> Result<LessSafeKey, CryptoError> {
    let unbound = UnboundKey::new(&AES_128_GCM, &secret.0)
        .map_err(|_| CryptoError::BadSecretLength { len: SECRET_LEN })?;
    Ok(LessSafeKey::new(unbound))
}

fn nonce(iv: [u8; IV_LEN]) -> Nonce {
    Nonce::assume_unique_for_key(iv)
}

/// Encrypt with a caller-supplied IV. Only for deterministic tests and for
/// vector reproduction — production code must use [`seal`], which draws a
/// fresh IV from the OS CSPRNG (§4.2 "must still use a CSPRNG").
pub fn seal_with_iv(
    secret: &Secret,
    plaintext: &[u8],
    iv: [u8; IV_LEN],
) -> Result<Vec<u8>, CryptoError> {
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(CryptoError::PlaintextTooLarge {
            len: plaintext.len(),
        });
    }
    let k = key(secret)?;
    let mut out = Vec::with_capacity(plaintext.len() + OVERHEAD);
    out.extend_from_slice(&iv);
    out.extend_from_slice(plaintext);
    let tag = k
        .seal_in_place_separate_tag(nonce(iv), Aad::empty(), &mut out[IV_LEN..])
        .map_err(|_| CryptoError::RandomSource)?;
    out.extend_from_slice(tag.as_ref());
    Ok(out)
}

/// Encrypt with a fresh random IV: returns `IV ‖ ciphertext ‖ tag`.
pub fn seal(secret: &Secret, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    seal_with_rng(secret, plaintext, &SystemRandom::new())
}

/// Like [`seal`], but with an injectable random source.
pub fn seal_with_rng(
    secret: &Secret,
    plaintext: &[u8],
    rng: &dyn SecureRandom,
) -> Result<Vec<u8>, CryptoError> {
    let mut iv = [0u8; IV_LEN];
    rng.fill(&mut iv).map_err(|_| CryptoError::RandomSource)?;
    seal_with_iv(secret, plaintext, iv)
}

/// Decrypt `IV ‖ ciphertext ‖ tag` and verify the tag.
///
/// Any authentication failure is returned as [`CryptoError::InvalidTag`]; the
/// caller should drop the datagram silently, exactly like the reference
/// implementation (§4.3).
pub fn open(secret: &Secret, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < OVERHEAD {
        return Err(CryptoError::BlobTooShort { len: blob.len() });
    }
    if blob.len() > MAX_BLOB_LEN {
        return Err(CryptoError::BlobTooLarge { len: blob.len() });
    }
    let mut iv = [0u8; IV_LEN];
    iv.copy_from_slice(&blob[..IV_LEN]);
    let mut buf = blob[IV_LEN..].to_vec();
    let k = key(secret)?;
    let plain = k
        .open_in_place(nonce(iv), Aad::empty(), &mut buf)
        .map_err(|_| CryptoError::InvalidTag)?;
    Ok(plain.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(n: u8) -> Secret {
        Secret::from_bytes([n; SECRET_LEN])
    }

    #[test]
    fn roundtrip_preserves_plaintext() {
        let s = secret(0x42);
        let plaintext = b"voice frame with \xff bytes";
        let blob = seal(&s, plaintext).unwrap();
        assert_eq!(blob.len(), plaintext.len() + OVERHEAD);
        assert_eq!(open(&s, &blob).unwrap(), plaintext);
    }

    #[test]
    fn empty_plaintext_is_legal() {
        let s = secret(1);
        let blob = seal(&s, &[]).unwrap();
        assert_eq!(blob.len(), OVERHEAD);
        assert_eq!(open(&s, &blob).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn blob_layout_is_iv_then_ciphertext_then_tag() {
        let s = secret(7);
        let iv = [9u8; IV_LEN];
        let blob = seal_with_iv(&s, b"abc", iv).unwrap();
        assert_eq!(&blob[..IV_LEN], &[9u8; IV_LEN]);
        // GCM is a stream mode: ciphertext length equals plaintext length.
        assert_eq!(blob.len(), IV_LEN + 3 + TAG_LEN);
    }

    #[test]
    fn encryption_is_deterministic_for_a_fixed_iv() {
        let s = secret(3);
        let a = seal_with_iv(&s, b"same", [1u8; IV_LEN]).unwrap();
        let b = seal_with_iv(&s, b"same", [1u8; IV_LEN]).unwrap();
        assert_eq!(a, b);
        let c = seal_with_iv(&s, b"same", [2u8; IV_LEN]).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn fresh_iv_every_packet() {
        let s = secret(0xAA);
        let a = seal(&s, b"frame").unwrap();
        let b = seal(&s, b"frame").unwrap();
        assert_ne!(&a[..IV_LEN], &b[..IV_LEN]);
        assert_ne!(a, b);
        // Both still decrypt.
        assert_eq!(open(&s, &a).unwrap(), b"frame");
        assert_eq!(open(&s, &b).unwrap(), b"frame");
    }

    #[test]
    fn tampered_tag_is_rejected() {
        let s = secret(5);
        let mut blob = seal(&s, b"hello").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert_eq!(open(&s, &blob), Err(CryptoError::InvalidTag));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let s = secret(5);
        let mut blob = seal(&s, b"hello world").unwrap();
        blob[IV_LEN + 2] ^= 0x80;
        assert_eq!(open(&s, &blob), Err(CryptoError::InvalidTag));
    }

    #[test]
    fn tampered_iv_is_rejected() {
        let s = secret(5);
        let mut blob = seal(&s, b"hello world").unwrap();
        blob[0] ^= 0x01;
        assert_eq!(open(&s, &blob), Err(CryptoError::InvalidTag));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let blob = seal(&secret(1), b"payload").unwrap();
        assert_eq!(open(&secret(2), &blob), Err(CryptoError::InvalidTag));
    }

    #[test]
    fn truncated_blob_is_rejected() {
        let s = secret(1);
        assert_eq!(
            open(&s, &[0u8; OVERHEAD - 1]),
            Err(CryptoError::BlobTooShort { len: OVERHEAD - 1 })
        );
        assert_eq!(open(&s, &[]), Err(CryptoError::BlobTooShort { len: 0 }));
    }

    #[test]
    fn oversized_blob_is_rejected() {
        let s = secret(1);
        assert_eq!(
            open(&s, &[0u8; MAX_BLOB_LEN + 1]),
            Err(CryptoError::BlobTooLarge {
                len: MAX_BLOB_LEN + 1
            })
        );
    }

    #[test]
    fn max_plaintext_roundtrip() {
        let s = secret(0x11);
        let plaintext = vec![0x5Au8; MAX_PLAINTEXT_LEN];
        let blob = seal(&s, &plaintext).unwrap();
        assert_eq!(blob.len(), MAX_BLOB_LEN);
        assert_eq!(open(&s, &blob).unwrap(), plaintext);
    }

    #[test]
    fn plaintext_one_byte_over_limit_is_refused() {
        let s = secret(0x11);
        assert_eq!(
            seal(&s, &vec![0u8; MAX_PLAINTEXT_LEN + 1]),
            Err(CryptoError::PlaintextTooLarge {
                len: MAX_PLAINTEXT_LEN + 1
            })
        );
    }

    #[test]
    fn secret_length_is_validated() {
        assert_eq!(
            Secret::from_slice(&[0u8; 15]),
            Err(CryptoError::BadSecretLength { len: 15 })
        );
        assert!(Secret::from_slice(&[0u8; 16]).is_ok());
    }

    #[test]
    fn generated_secrets_differ_and_are_not_zero() {
        let a = Secret::generate().unwrap();
        let b = Secret::generate().unwrap();
        assert_ne!(a, b);
        assert_ne!(a.as_bytes(), &[0u8; SECRET_LEN]);
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        let s = Secret::from_bytes([0xAB; SECRET_LEN]);
        let text = format!("{s:?}");
        assert!(!text.contains("ab"), "{text}");
        assert_eq!(text, "Secret(**redacted**)");
    }
}
