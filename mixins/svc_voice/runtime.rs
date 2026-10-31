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

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
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
static REQUESTED: AtomicBool = AtomicBool::new(false);

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
pub fn take_secret_request() -> Option<Vec<u8>> {
    if REQUESTED.swap(true, Ordering::Relaxed) {
        return None;
    }
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
    REQUESTED.store(false, Ordering::Relaxed);
    stop();
}

// --- the session thread -----------------------------------------------------

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static STATE: Mutex<Option<String>> = Mutex::new(None);
static COUNTERS: Mutex<Option<String>> = Mutex::new(None);
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

                let mut mic = svc_engine::audio::Capture::open(None)
                    .map_err(|e| format!("microphone: {e}"))?;
                let sink = svc_engine::audio::Playback::open(None)
                    .map_err(|e| format!("output: {e}"))?;
                while !STOP.load(Ordering::SeqCst) {
                    // The pipeline does not read the socket: the caller pumps,
                    // which is also what drives the session's timers.
                    if let Err(e) = client.pump() {
                        return Err(format!("pump: {e}"));
                    }
                    pipe.tick(std::time::Instant::now(), &mut client, &mut mic, &sink);
                    *STATE.lock().unwrap() = Some(format!("{:?}", client.state()));
                    let s = client.stats();
                    *COUNTERS.lock().unwrap() = Some(format!(
                        "auth {} cc {} mic {} snd {}",
                        s.auth_sent, s.connection_check_sent, s.mic_frames_sent, s.sound_frames
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
