# Third-party notices

## opus-rs

`mixins/svc_voice/engine/src/opus/` is a vendored copy of the `opus-rs` crate, version 0.1.40,
a pure-Rust implementation of the Opus codec (RFC 6716). It is not modified beyond module plumbing:
`lib.rs` became `mod.rs` and internal `crate::` paths became `crate::opus::`.

Upstream: https://crates.io/crates/opus-rs

Its licence text is in `COPYING-opus-rs.txt` (BSD-3-Clause, restsend.com, plus the Xiph.Org /
Skype Limited / Octasic / CSIRO notices it carries). It has to live outside the `.hnmod` bundle
because the registry only accepts `.rs`, `.toml` and `data/` inside a mod.

## Horizon

The mod links against `horizon-client`'s own dependencies (`ring`, `cpal`); nothing of Horizon is
vendored here. `tools/package.py` comes from `horizon-rust/horizon-native-mod-template`.
