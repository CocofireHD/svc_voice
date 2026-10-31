// Voice chat runtime, injected into the client as `crate::game::svc_voice`.
//
// What it holds today is the state the voice loop needs from the game: the
// listener's eye position and look direction, published by the tick inject and
// read by the audio thread without locking.
//
// The loop itself lives in the bundle's own crate (`svc_engine`, staged to
// `native-crates/svc_voice/engine/`): protocol, crypto, jitter, volume, and
// Opus, the last vendored as source because Horizon's `Cargo.lock` has no
// codec crate and a bundle may not add one.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::collections::HashMap;
use std::sync::Mutex;

/// The engine is a real dependency, not a placeholder: this is its frame size,
/// and the compiler fails the bundle if the crate is missing from the stage.
pub const ENGINE_FRAME_SAMPLES: usize = svc_engine::audio::FRAME_SAMPLES;

/// Eye position in thousandths of a block, so `AtomicI64` can carry it.
static EYE: [AtomicI64; 3] = [AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)];
/// Look direction as thousandths of a unit vector.
static LOOK: [AtomicI64; 3] = [AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(1000)];
/// Frames the game ticked since the mod loaded.
static TICKS: AtomicU32 = AtomicU32::new(0);

fn to_milli(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

fn from_milli(v: i64) -> f64 {
    v as f64 / 1000.0
}

// Player positions, for distance attenuation (§7.4). The voice packet names
// the speaker by UUID and the world names entities by id, so both directions
// are kept: id -> UUID from `AddEntity`, UUID -> position from the movement
// packets. Keyed by the UUID's 16 big-endian bytes, which is what
// `Uuid::as_bytes` returns, without needing the uuid crate here.
static ENTITY_IDS: Mutex<Option<HashMap<i32, [u8; 16]>>> = Mutex::new(None);
static POSITIONS: Mutex<Option<HashMap<[u8; 16], [f64; 3]>>> = Mutex::new(None);

/// The 16 bytes of a protocol UUID, high half first — the same order as the
/// `u128` in `AddEntity`.
fn uuid_key(u: &svc_engine::protocol::Uuid) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&u.msb.to_be_bytes());
    key[8..].copy_from_slice(&u.lsb.to_be_bytes());
    key
}

pub fn map_entity(id: i32, key: [u8; 16]) {
    ENTITY_IDS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(id, key);
}

pub fn entity_key(id: i32) -> Option<[u8; 16]> {
    ENTITY_IDS.lock().unwrap().as_ref()?.get(&id).copied()
}

pub fn forget_entity(id: i32) -> Option<[u8; 16]> {
    ENTITY_IDS.lock().unwrap().as_mut()?.remove(&id)
}

pub fn set_entity(key: [u8; 16], position: [f64; 3]) {
    POSITIONS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, position);
}

pub fn remove_entity(key: [u8; 16]) {
    if let Some(map) = POSITIONS.lock().unwrap().as_mut() {
        map.remove(&key);
    }
}

pub fn entity_position(key: &[u8; 16]) -> Option<[f64; 3]> {
    POSITIONS.lock().unwrap().as_ref()?.get(key).copied()
}

/// How many speakers the client currently knows a position for.
pub fn known_speakers() -> usize {
    POSITIONS.lock().unwrap().as_ref().map_or(0, HashMap::len)
}

/// The push-to-talk key, as the tick sees it.
pub fn set_ptt(held: bool) {
    PTT.store(held, Ordering::Relaxed);
}

pub fn ptt_held() -> bool {
    PTT.load(Ordering::Relaxed)
}

/// 0 voice activity, 1 always on, 2 push-to-talk.
pub fn gate_mode() -> svc_engine::voice::Gate {
    match GATE.load(Ordering::Relaxed) {
        1 => svc_engine::voice::Gate::Open,
        2 => svc_engine::voice::Gate::PushToTalk,
        _ => svc_engine::voice::Gate::VoiceActivity,
    }
}

fn gate_name(mode: u8) -> &'static str {
    match mode {
        1 => "always-on",
        2 => "push-to-talk",
        _ => "voice-activity",
    }
}

/// Cycle the gate on a key press; returns the new mode's name once per press.
pub fn cycle_gate(held: bool) -> Option<&'static str> {
    if !held {
        GATE_HELD.store(false, Ordering::Relaxed);
        return None;
    }
    if GATE_HELD.swap(true, Ordering::Relaxed) {
        return None;
    }
    let next = (GATE.load(Ordering::Relaxed) + 1) % 3;
    GATE.store(next, Ordering::Relaxed);
    Some(gate_name(next))
}

pub fn gate_name_now() -> &'static str {
    gate_name(GATE.load(Ordering::Relaxed))
}

/// True on the frame the key goes down, once per press.
pub fn mute_key_edge(held: bool) -> bool {
    if held {
        !MUTE_HELD.swap(true, Ordering::Relaxed)
    } else {
        MUTE_HELD.store(false, Ordering::Relaxed);
        false
    }
}

/// Flip the microphone and report the new state: `true` means disabled.
pub fn toggle_mute() -> bool {
    let now = !MUTED.load(Ordering::Relaxed);
    MUTED.store(now, Ordering::Relaxed);
    now
}

pub fn is_muted() -> bool {
    MUTED.load(Ordering::Relaxed)
}

/// Publish where the listener is and where it looks. Called from the tick
/// inject; cheap enough for every frame.
pub fn set_listener(eye: [f64; 3], look: [f64; 3]) {
    for i in 0..3 {
        EYE[i].store(to_milli(eye[i]), Ordering::Relaxed);
        LOOK[i].store(to_milli(look[i]), Ordering::Relaxed);
    }
}

/// The listener's eye position, as last published.
pub fn listener_eye() -> [f64; 3] {
    [from_milli(EYE[0].load(Ordering::Relaxed)),
     from_milli(EYE[1].load(Ordering::Relaxed)),
     from_milli(EYE[2].load(Ordering::Relaxed))]
}

/// The listener's look direction, as last published.
pub fn listener_look() -> [f64; 3] {
    [from_milli(LOOK[0].load(Ordering::Relaxed)),
     from_milli(LOOK[1].load(Ordering::Relaxed)),
     from_milli(LOOK[2].load(Ordering::Relaxed))]
}

pub fn ticks() -> u32 {
    TICKS.load(Ordering::Relaxed)
}

/// Head of the client's per-frame tick.
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// What the server's `voicechat:secret` payload told us: everything the UDP
/// session needs, kept game-free so this file still builds on its own.
#[derive(Clone, Debug)]
pub struct SecretInfo {
    pub secret: [u8; 16],
    pub server_port: i32,
    pub voice_host: String,
    pub mtu: i32,
    pub distance: f64,
    pub keep_alive: i32,
}

static SECRET: Mutex<Option<SecretInfo>> = Mutex::new(None);
// The full payload as sent: `ClientSession::connect` is armed from it.
static PAYLOAD: Mutex<Option<svc_engine::protocol::handshake::SecretPayload>> = Mutex::new(None);
static SECRET_ERROR: Mutex<Option<String>> = Mutex::new(None);
static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static LAST_REQUEST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// One per three seconds while the server says nothing; a server without the
/// plugin then hears five packets, not an endless stream.
const MAX_ATTEMPTS: u32 = 5;

/// `voicechat:secret` arrives on the plugin channel; decode and keep it.
/// Returns true when the channel was ours, so the caller can stop looking.
pub fn on_plugin_message(channel: &str, data: &[u8]) -> bool {
    if channel != "voicechat:secret" {
        return false;
    }
    match svc_engine::protocol::handshake::SecretPayload::decode(data) {
        Ok(p) => {
            *PAYLOAD.lock().unwrap() = Some(p.clone());
            *SECRET.lock().unwrap() = Some(SecretInfo {
                secret: p.secret,
                server_port: p.server_port,
                voice_host: p.voice_host.clone(),
                mtu: p.mtu_size,
                distance: p.voice_chat_distance,
                keep_alive: p.keep_alive,
            });
            *SECRET_ERROR.lock().unwrap() = None;
        }
        Err(e) => *SECRET_ERROR.lock().unwrap() = Some(format!("{e:?}")),
    }
    true
}

/// The bytes to send on `voicechat:request_secret`, once.
/// The secret request, retried a few times before giving up. One shot is not
/// enough: the first frame with a connection can be before the server accepts
/// play packets, and a dropped request used to leave the voice session dead
/// for the whole login (seen in a run where two clients joined at once).
pub fn take_secret_request() -> Option<Vec<u8>> {
    if PAYLOAD.lock().unwrap().is_some() || ATTEMPTS.load(Ordering::Relaxed) >= MAX_ATTEMPTS {
        return None;
    }
    let now = std::time::Instant::now();
    {
        let mut last = LAST_REQUEST.lock().unwrap();
        if let Some(at) = *last {
            if now.duration_since(at) < std::time::Duration::from_secs(3) {
                return None;
            }
        }
        *last = Some(now);
    }
    ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    Some(svc_engine::protocol::handshake::RequestSecret::new().encode())
}

pub fn secret() -> Option<SecretInfo> {
    SECRET.lock().unwrap().clone()
}

pub fn secret_error() -> Option<String> {
    SECRET_ERROR.lock().unwrap().clone()
}

/// Dropped on disconnect so a reconnect asks again.
pub fn reset_session() {
    *SECRET.lock().unwrap() = None;
    *PAYLOAD.lock().unwrap() = None;
    ATTEMPTS.store(0, Ordering::Relaxed);
    *LAST_REQUEST.lock().unwrap() = None;
    stop();
}

// --- the session thread -----------------------------------------------------

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static STATE: Mutex<Option<String>> = Mutex::new(None);
static COUNTERS: Mutex<Option<String>> = Mutex::new(None);
// Voice activity by default; V cycles it, CapsLock is push-to-talk.
static PTT: AtomicBool = AtomicBool::new(false);
static GATE: AtomicU8 = AtomicU8::new(0);
static GATE_HELD: AtomicBool = AtomicBool::new(false);
// §8.1: the microphone switch, and the key edge that flips it.
static MUTED: AtomicBool = AtomicBool::new(false);
static MUTE_HELD: AtomicBool = AtomicBool::new(false);
// Backoff: a session that dies must not be retried every frame.
static LAST_ATTEMPT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn host_of(address: &str) -> String {
    match address.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_string(),
        _ => address.to_string(),
    }
}

/// Start the UDP session once, now that the server told us where it is.
/// `mc_address` is the Minecraft server address, used only when the server did
/// not send a `voice_host` of its own (§2.3.1).
pub fn ensure_started(mc_address: &str) -> Option<String> {
    let payload = PAYLOAD.lock().unwrap().clone()?;
    {
        let now = std::time::Instant::now();
        let mut last = LAST_ATTEMPT.lock().unwrap();
        if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(5)) {
            return None;
        }
        *last = Some(now);
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return None;
    }
    let host = if payload.voice_host.trim().is_empty() {
        host_of(mc_address)
    } else {
        payload.voice_host.clone()
    };
    let endpoint = format!("{host}:{}", payload.server_port);
    STOP.store(false, Ordering::SeqCst);
    *LAST_ERROR.lock().unwrap() = None;
    let endpoint_for_thread = endpoint.clone();

    std::thread::Builder::new()
        .name("svc-voice".into())
        .spawn(move || {
            let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
                use svc_engine::client::{ClientConfig, ClientSession};
                use svc_engine::voice::{PipelineConfig, VoicePipeline};

                let mut client = ClientSession::connect(
                    endpoint_for_thread.as_str(),
                    &payload,
                    ClientConfig::default(),
                )
                .map_err(|e| format!("connect {endpoint_for_thread}: {e}"))?;
                let mut pipe = VoicePipeline::new(PipelineConfig::default())?;
                pipe.attach(&mut client);

                // Device override for testing and for players with two sound
                // cards: SVC_INPUT_DEVICE / SVC_OUTPUT_DEVICE, substring match.
                let input = std::env::var("SVC_INPUT_DEVICE").ok();
                let output = std::env::var("SVC_OUTPUT_DEVICE").ok();
                let mut mic = svc_engine::audio::Capture::open(input.as_deref())
                    .map_err(|e| format!("microphone: {e}"))?;
                let sink = svc_engine::audio::Playback::open(output.as_deref())
                    .map_err(|e| format!("output: {e}"))?;
                // SVC_GATE picks the first mode: voice-activity (default),
                // always-on, or push-to-talk.
                if let Ok(name) = std::env::var("SVC_GATE") {
                    GATE.store(
                        match name.as_str() {
                            "always" | "always-on" | "open" => 1,
                            "ptt" | "push-to-talk" => 2,
                            _ => 0,
                        },
                        Ordering::Relaxed,
                    );
                }
                while !STOP.load(Ordering::SeqCst) {
                    // The pipeline does not read the socket: the caller pumps,
                    // which is also what drives the session's timers.
                    pipe.set_gate(gate_mode());
                    pipe.set_ptt(ptt_held());
                    if let Err(e) = client.pump() {
                        return Err(format!("pump: {e}"));
                    }
                    pipe.tick_positional(
                        std::time::Instant::now(),
                        &mut client,
                        &mut mic,
                        &sink,
                        listener_eye(),
                        listener_look(),
                        |u| entity_position(&uuid_key(&u)),
                    );
                    *STATE.lock().unwrap() = Some(format!("{:?}", client.state()));
                    let s = client.stats();
                    *COUNTERS.lock().unwrap() = Some(format!(
                        "auth {} cc {} mic {} skip {} lvl {} snd {} att {} pan {} pos {} gate {} {}",
                        s.auth_sent,
                        s.connection_check_sent,
                        s.mic_frames_sent,
                        pipe.stats().mic_skipped,
                        pipe.level(),
                        s.sound_frames,
                        pipe.attenuated(),
                        pipe.panned(),
                        known_speakers(),
                        gate_name_now(),
                        if ptt_held() { "ptt" } else { "" }
                    ));
                    if client.needs_secret() {
                        return Err("server asked for a new secret".into());
                    }
                }
                Ok(())
            }));
            match started {
                Ok(Ok(())) => {}
                Ok(Err(e)) => *LAST_ERROR.lock().unwrap() = Some(e),
                Err(panic) => {
                    let text = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic".into());
                    *LAST_ERROR.lock().unwrap() = Some(format!("panicked: {text}"));
                }
            }
            RUNNING.store(false, Ordering::SeqCst);
        })
        .ok()?;
    Some(endpoint)
}

pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
}

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().clone()
}

/// The last error, once: the tick logs it and the message does not repeat.
pub fn take_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().take()
}

pub fn session_state() -> Option<String> {
    STATE.lock().unwrap().clone()
}

pub fn take_counters_change() -> Option<String> {
    static LAST: Mutex<Option<String>> = Mutex::new(None);
    let now = COUNTERS.lock().unwrap().clone();
    if now.is_none() || now == *LAST.lock().unwrap() {
        return None;
    }
    *LAST.lock().unwrap() = now.clone();
    now
}

/// The state string, once per change, for the tick to log.
pub fn take_state_change() -> Option<String> {
    static LAST_LOGGED: Mutex<Option<String>> = Mutex::new(None);
    let now = STATE.lock().unwrap().clone();
    let mut last = LAST_LOGGED.lock().unwrap();
    if now == *last {
        return None;
    }
    *last = now.clone();
    now
}

/// Geometry the future mixer needs: the angle between the listener's look
/// vector and the direction to a sound, in radians, clamped to `[0, π]`.
pub fn angle_to_sound(eye: [f64; 3], look: [f64; 3], sound: [f64; 3]) -> f32 {
    let d = [sound[0] - eye[0], sound[1] - eye[1], sound[2] - eye[2]];
    let dn = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let ln = (look[0] * look[0] + look[1] * look[1] + look[2] * look[2]).sqrt();
    if dn == 0.0 || ln == 0.0 {
        return 0.0;
    }
    let dot = (d[0] * look[0] + d[1] * look[1] + d[2] * look[2]) / (dn * ln);
    dot.clamp(-1.0, 1.0).acos() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mute_key_fires_once_per_press() {
        assert!(mute_key_edge(true));
        assert!(!mute_key_edge(true));
        assert!(!mute_key_edge(false));
        assert!(mute_key_edge(true));
        assert!(!mute_key_edge(false));
    }

    #[test]
    fn toggling_mute_flips_the_byte_the_server_gets() {
        let before = is_muted();
        assert_eq!(toggle_mute(), !before);
        assert_eq!(is_muted(), !before);
        assert_eq!(toggle_mute(), before);
        assert_eq!(is_muted(), before);
    }

    #[test]
    fn the_published_position_round_trips() {
        set_listener([12.5, 64.0, -3.25], [0.0, 0.0, 1.0]);
        assert_eq!(listener_eye(), [12.5, 64.0, -3.25]);
        assert_eq!(listener_look(), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn ticks_advance() {
        let before = ticks();
        on_tick();
        assert!(ticks() > before);
    }

    #[test]
    fn the_angle_is_zero_ahead_and_pi_behind() {
        let eye = [0.0, 0.0, 0.0];
        let look = [0.0, 0.0, 1.0];
        assert!(angle_to_sound(eye, look, [0.0, 0.0, 5.0]) < 1e-5);
        let behind = angle_to_sound(eye, look, [0.0, 0.0, -5.0]);
        assert!((behind - std::f32::consts::PI).abs() < 1e-4, "{behind}");
        let right = angle_to_sound(eye, look, [5.0, 0.0, 0.0]);
        assert!((right - std::f32::consts::FRAC_PI_2).abs() < 1e-4, "{right}");
    }

    #[test]
    fn a_sound_at_the_ear_has_no_angle() {
        assert_eq!(angle_to_sound([1.0, 2.0, 3.0], [0.0, 0.0, 1.0], [1.0, 2.0, 3.0]), 0.0);
    }
}
