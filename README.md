# svc_voice — a Horizon native mod (bundle, not yet a build)

This folder is the Simple Voice Chat client packaged the way Horizon's market takes it: a source-only
`.hnmod` that the launcher compiles against a pinned Horizon revision.

```bash
python tools/package.py     # -> target/svc_voice-0.1.0.hnmod
```

Current artefact: `target/svc_voice-0.1.0.hnmod`, 353 263 bytes, 91 entries,
sha256 `c7379ba53a3eac0742e1501c2074c14f86455c8b34534d6fb4db66a55968597d`.
Built from a clean clone of this repository, so the hash is reproducible: LF endings, same files,
same order.

## What is in it

- `horizon-native.json` — schema 1, id `svc_voice`, version 0.1.0, game 26.3, Horizon
  `ba6344a5…`, horizon-mixins `d9c83d70…`
- `mixins/svc_voice/horizon-mixin.toml` — one `[[module]]`, one `[[crate]]`, two `[[injection]]`s
- `mixins/svc_voice/runtime.rs` — the module injected as `crate::game::svc_voice`: listener
  position snapshot (lock-free atomics), look-angle maths, and the voice secret (decoded from
  `voicechat:secret`, kept game-free so it still builds alone), with unit tests
- `mixins/svc_voice/tick.rs` — HEAD on `Game::update_world`: publishes eye + look every frame, and
  asks the server for the voice secret once, when connected
- `mixins/svc_voice/packet.rs` — HEAD on `Game::handle_packet`: decodes `voicechat:secret` into the
  module and leaves every other packet untouched
- `mixins/svc_voice/engine/` — the bundle crate `svc-engine`: protocol, crypto, client, audio,
  voice, and **Opus vendored as source** (`opus/`, `opus-rs` 0.1.40, BSD-3-Clause, 70 files,
  ~36 400 lines). It builds with `ring` 0.17 and optional `cpal` 0.16 — both already in Horizon's
  `Cargo.lock`, which is the only thing a bundle crate may depend on.

`cargo test` in `engine/` runs 228 tests (our protocol/client/audio/voice suites plus the vendored
codec's), all green.

## The injection target was checked, not guessed

`horizon_client::game::Game<'_>::tick` **does not exist** at the pinned revision — the first draft
used it and would not have built. `Game::update_world(&mut self)` does (it is the per-frame world
update, and it already reads `self.player`), so the injection moved there with the binding
`&mut crate::game::Game<'_> = &mut *self`.

## What is missing, precisely

The bundle now carries everything it needs. A bundle *may* ship its own crate (`[[crate]]`, staged
to `native-crates/<id>/<crate>/`); the one rule is that every dependency must already be in
Horizon's `Cargo.lock`, otherwise `E_CRATE_DEPENDENCY`. `ring` 0.17.14 and `cpal` 0.16.0 are in that
lock. There is no Opus crate in it, which is why Opus is vendored as source instead: no C
toolchain, no download, `opus-rs` under its BSD-3-Clause.

Checked against the real pinned source (`ba6344a5…`, a 106 MB checkout) with the real tool
(`horizon-mixins` 0.6.0):

- `plan` → `status: ok, crates: 1, modules: 1, injections: 2`
- `apply` → the helpers land at the head of `update_world` and `handle_packet`
  (`svc_voice_tick(&mut *self)`, `svc_voice_packet(&mut *self, &packet)`), and the crate is staged
  at `native-crates/svc_voice/engine/`
- `cargo check -p svc-engine --offline` inside the staged workspace → builds
- `cargo check -p horizon-client --offline` → **Finished, exit 0** (also with the audio devices
  enabled, i.e. with `cpal` linked into the client)

A detail worth knowing: a method defined in a submodule needs the *submodule* in the target path —
`horizon_client::game::Game<'_>::handle_packet` is not found, but
`horizon_client::game::remote::Game<'_>::handle_packet` is.

Two errors this caught, both invisible in a standalone build: a `Cargo.lock` inside the bundle
crate (`E_CRATE`: a bundle crate builds against Horizon's lock, it may not ship its own) and `//!`
inner doc comments in the injected module (`E_MODULE`: support module source cannot carry file
attributes).

The session now runs: on the first tick after the secret arrives, the module opens the UDP socket on
its own thread (`ClientSession::connect` → `VoicePipeline` → `Capture`/`Playback`), stops on
disconnect, and records the state string for the next step.

Still open: distance volume in the live path (the pipeline's `tick` delivers without a gain yet;
`deliver_with_gain` needs the listener position, which the module already publishes), a way to see
what the engine is doing in game (no HUD button, no keybind), and the listing itself — public repo,
signed release, registry PR.

## It works in the game

Joined a real Paper 26.3 + Simple Voice Chat 2.6.27 server from the Horizon client, headless through
the client's own `HORIZON_NET_REPORT` harness, with the bundle staged and compiled in:

```
client: svc_voice: voice session to 127.0.0.1:24454
        svc_voice: auth 1 cc 1 mic 0 snd 0
        svc_voice: state Validated
        svc_voice: auth 1 cc 1 mic 1 snd 0      (mic frames going out)
server: Received secret request of Player56099 (20)
        Sent secret to Player56099
        Successfully authenticated player 116b0732-…
        Successfully validated connection of player 116b0732-…
        Player Player56099 (116b0732-…) successfully connected to voice chat
```

### Positional audio

The voice packet names the speaker by UUID and carries no position, so the mod tracks entities
itself: `AddEntity` gives id and UUID, `TeleportEntity` and `EntityPositionSync` give movement,
`RemoveEntities` forgets. `VoicePipeline::tick_positional` then attenuates every sound with the
curve from §7.4 — `1 - distance / max`, cut at `max + 1`, with `max` taken from the packet.

Measured in game, two clients 21.5 blocks apart with the server's 48-block radius:

```
SvcAttA   snd 5204  att 5204      (gain ~0.55 on every frame)
SvcAttB   snd 5212  att 5205
```

`att` counts sounds that were played below full volume; it equals `snd`, so the curve ran on every
single frame rather than being skipped.

The bug that had to be fixed to get there: `VoicePipeline::tick` never reads the socket — the caller
must `client.pump()`. Without it the client authenticated on the wire but never saw the server's
reply, so it sat in `Authenticating` with `connection_check_sent == 0` while the server had already
logged the login. One line in the session thread, and the state reached `Validated`.

Two things about the harness itself, worth knowing (both pre-existing, not caused by the mod):

- the headless client **overflows the main thread's stack on Windows**, even unmodded — a 64 MB
  thread around `game::run()` is enough to get past it (test-only patch, not shipped);
- it disconnects about six seconds after joining, so a session test has to complete inside that
  window.

## Before publishing

- Build it once with `python tools/horizon.py check` on the pinned revision, then `build`.
- Pin a **published** Horizon revision, not a local one.
- GitHub repository + release with the `.hnmod` (and a minisign signature), then the listing PR in
  `horizon-rust/horizon-mods`.
