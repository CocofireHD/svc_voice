//! Clean-room reimplementation of the Simple Voice Chat (SVC) wire protocol.
//!
//! This crate contains **only** in-memory protocol logic: byte-level
//! encoding/decoding, AES-128-GCM, datagram framing, the Minecraft custom
//! payloads used for discovery, the UDP session state machine and a jitter
//! buffer. There is deliberately no socket, no audio I/O and no threading, so
//! that the whole crate is testable without a network or a clock.
//!
//! Everything here was derived exclusively from `SPEC.md`; the Java
//! implementation was never read. See `README.md` for the clean-room
//! statement and the list of documented spec ambiguities.
//!
//! # Module map
//!
//! | module | contents |
//! |---|---|
//! | [`wire`] | VarInt, String, `byte[]`, UUID and fixed-width primitives |
//! | [`crypto`] | AES-128-GCM with `IV‖ciphertext‖tag` framing |
//! | [`framing`] | UDP datagram header (client and server direction) |
//! | [`packets`] | the ten UDP packet types `0x01`…`0x0A` |
//! | [`handshake`] | `voicechat:request_secret` / `voicechat:secret` payloads |
//! | [`session`] | pure state machine: retries, keep-alives, timeout |
//! | [`jitter`] | per-speaker reordering buffer with explicit bounds |
//!
//! # Typical use
//!
//! ```
//! use svc_engine::protocol::{crypto::Secret, packets::{Mic, Packet}, wire::Uuid};
//!
//! let secret = Secret::from_bytes([7u8; 16]);
//! let me = Uuid::from_parts(1, 2);
//! let mic = Packet::Mic(Mic { data: vec![0xAA; 40], sequence: 0, whispering: false });
//!
//! let datagram = svc_engine::protocol::encode_outgoing(me, &secret, &mic).unwrap();
//! // ... send datagram over UDP ...
//! let (sender, packet) = svc_engine::protocol::decode_incoming_client(&secret, &datagram).unwrap();
//! assert_eq!(sender, me);
//! assert_eq!(packet, mic);
//!
//! // Server -> client datagrams have no UUID field (§3.2):
//! let blob = svc_engine::protocol::crypto::seal(&secret, &packet.encode().unwrap()).unwrap();
//! let datagram = svc_engine::protocol::framing::encode_server(&blob).unwrap();
//! assert_eq!(svc_engine::protocol::decode_incoming(&secret, &datagram).unwrap(), mic);
//! ```

pub mod crypto;
pub mod framing;
pub mod handshake;
pub mod jitter;
pub mod packets;
pub mod session;
pub mod wire;

use std::fmt;

pub use crypto::Secret;
pub use framing::{MAGIC, MAX_BLOB_LEN, MAX_DATAGRAM_LEN};
pub use handshake::COMPATIBILITY_VERSION;
pub use packets::Packet;
pub use wire::Uuid;

/// Errors produced by the convenience helpers in this module.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Packet(packets::PacketError),
    Crypto(crypto::CryptoError),
    Frame(framing::FrameError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Packet(e) => write!(f, "packet: {e}"),
            Error::Crypto(e) => write!(f, "crypto: {e}"),
            Error::Frame(e) => write!(f, "framing: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<packets::PacketError> for Error {
    fn from(e: packets::PacketError) -> Self {
        Error::Packet(e)
    }
}

impl From<crypto::CryptoError> for Error {
    fn from(e: crypto::CryptoError) -> Self {
        Error::Crypto(e)
    }
}

impl From<framing::FrameError> for Error {
    fn from(e: framing::FrameError) -> Self {
        Error::Frame(e)
    }
}

/// Full outbound path: serialise a packet, encrypt it, frame it for the server.
///
/// Mirrors the send order of SPEC §4.3: `id+fields` → encrypt with a fresh
/// random IV → prepend IV/append tag → `0xFF ‖ uuid ‖ VarInt len ‖ blob`.
pub fn encode_outgoing(uuid: Uuid, secret: &Secret, packet: &Packet) -> Result<Vec<u8>, Error> {
    let plaintext = packet.encode()?;
    let blob = crypto::seal(secret, &plaintext)?;
    Ok(framing::encode_client(uuid, &blob)?)
}

/// Full inbound path for a server→client datagram: deframe, decrypt, parse.
pub fn decode_incoming(secret: &Secret, datagram: &[u8]) -> Result<Packet, Error> {
    let blob = framing::decode_server(datagram)?;
    let plaintext = crypto::open(secret, blob)?;
    Ok(Packet::decode(&plaintext)?)
}

/// Full inbound path for a client→server datagram (used by tests and by a
/// future self-check / loopback mode).
pub fn decode_incoming_client(secret: &Secret, datagram: &[u8]) -> Result<(Uuid, Packet), Error> {
    let (uuid, blob) = framing::decode_client(datagram)?;
    let plaintext = crypto::open(secret, blob)?;
    Ok((uuid, Packet::decode(&plaintext)?))
}
