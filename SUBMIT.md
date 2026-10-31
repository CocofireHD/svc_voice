# Submitting to the Horizon registry

The registry takes the file through its form, not a bare pull request: the checks download each
entry's file from a release in `horizon-rust/horizon-mods`, and only the form (its bot) can write
there. So this page is the copy for the form at <https://playhorizon.dev/mods/submit>.

## File

`svc_voice-0.1.0.hnmod` from the [v0.1.0 release](https://github.com/CocofireHD/svc_voice/releases/tag/v0.1.0),
with its `.minisig` beside it.

## The fields

| Field | Value |
| --- | --- |
| id | `svc_voice` |
| kind | `native-mod` |
| name | Simple Voice Chat |
| summary | Proximity voice chat for Horizon, speaking the Simple Voice Chat protocol. |
| authors | `CocofireHD` |
| owners | `CocofireHD` |
| license | MIT |
| source | https://github.com/CocofireHD/svc_voice |
| icon | `icons/svc_voice.png` |
| public key | `RWShVhK8k7rY2j8oOAiDUIOI0R9iloJlXHe+muadSHVUNqiA0/aRjKz1` |
| version | 0.1.0 |
| game | 26.3 |

### Description

```
Voice chat on a server that runs Simple Voice Chat, built from the protocol up in Rust.

The mod asks the server for its secret over the plugin channel, opens the UDP session on its own
thread, and encodes and decodes Opus in pure Rust — no C library, nothing to install. Sounds are
played at the volume their distance says, with the radius the server sends. M mutes your
microphone.

Bundles the Horizon and Horizon Mixins revisions it was built and checked against. On a server
without the plugin it does nothing at all.
```

### Changelog

```
0.1.0 — first release: handshake, UDP session, Opus, distance attenuation, mute.
```

## sha256

```
e386373c1a9a0b55728d42b809faf21de8e63a7c66e1d9d954ebd96fefe153c7
```
