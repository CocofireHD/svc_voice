//! Simple Voice Chat engine, shipped inside a Horizon native mod bundle.
//!
//! The bundle crate is staged to `native-crates/svc_voice/engine/` and compiled
//! against Horizon's own lockfile, which is why the only registry dependencies
//! are `ring` (0.17.14) and `cpal` (0.16.0) — both already pinned there — and
//! why Opus is vendored as source under `opus/` instead of pulled from crates.io.

//! * `protocol` — wire format, crypto, handshake, packets (no I/O)
//! * `client`   — the UDP session: transport and the state machine around it
//! * `audio`    — Opus encode/decode, plus capture/playback behind `device`
//! * `voice`    — the tick that joins them: jitter, concealment, distance volume
//! * `opus`     — vendored `opus-rs` 0.1.40 (BSD-3-Clause), encoder and decoder

pub mod opus;

pub mod protocol;
pub mod client;
pub mod audio;
pub mod voice;
