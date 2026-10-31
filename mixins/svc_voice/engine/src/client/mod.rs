//! `svc-client` — a live UDP voice client for the Simple Voice Chat protocol.
//!
//! Phase 2 of the clean-room reimplementation: the socket layer around the
//! pure protocol core of `svc-protocol`. There is still no audio I/O and no
//! Opus — [`ClientSession::push_mic`] takes an already-encoded frame, and
//! [`Incoming::Sound`] hands encoded frames out.
//!
//! ```no_run
//! use svc_engine::client::{ClientConfig, ClientSession, Incoming};
//! use crate::protocol::handshake::SecretPayload;
//!
//! # fn f(payload: SecretPayload) -> Result<(), Box<dyn std::error::Error>> {
//! let mut client = ClientSession::connect("127.0.0.1:24454", &payload, ClientConfig::default())?;
//! let inbox = client.take_inbox().expect("first call");
//! // ... drive the socket from the voice thread ...
//! client.pump()?;
//! for event in inbox.try_iter() {
//!     if let Incoming::Sound(frame) = event {
//!         // decode `frame.data`, position it by `frame.sender`/`frame.distance`
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! Design notes:
//!
//! * One socket, bound once, `connect()`ed to the voice endpoint for the whole
//!   session — the server keys a connection on the source address (§1, §6.3).
//! * [`ClientSession::pump`] is the whole event loop: read, drive the state
//!   machine, send what is due. It blocks at most `read_timeout` (20 ms).
//! * The mic side is an outbox with 20 ms pacing and the 500 ms TTL of §5; the
//!   receive side is a bounded channel of [`Incoming`] events.
//! * No `tokio`, no `mio`, no `log`: errors are counted in [`Stats`] and, for
//!   individual datagrams, reported as [`Incoming::Dropped`].

pub mod session;
pub mod transport;

pub use session::{
    ClientConfig, ClientError, ClientSession, DropReason, Incoming, SoundFrame, Stats,
};
pub use transport::{TransportError, TransportStats, UdpTransport};
