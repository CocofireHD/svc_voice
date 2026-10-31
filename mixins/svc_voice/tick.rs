// Runs at the head of the client's per-frame world update.
//
// The binding gives the whole `Game`, so the listener's eye position and look
// direction are published here without the voice thread ever touching the game.
// `Game` carries `player: Option<PlayerMotion>` (position), plus `yaw`, `pitch`
// and `eye` (eye height); the look vector is the same one `view_vector` in
// game.rs builds.

fn svc_voice_tick(game: &mut crate::game::Game<'_>) {
    // The settings file, read once per process; the keys it names are mapped
    // here because this module is the only place that knows winit's KeyCode.
    static SETTINGS: std::sync::OnceLock<crate::game::svc_voice::Config> =
        std::sync::OnceLock::new();
    let settings = SETTINGS.get_or_init(crate::game::svc_voice::config);
    let key_of = |name: &str| -> Option<KeyCode> {
        let upper = name.trim().to_ascii_uppercase();
        let mut chars = upper.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        if let Some(c) = single {
            return Some(match c {
                'A' => KeyCode::KeyA,
                'B' => KeyCode::KeyB,
                'C' => KeyCode::KeyC,
                'D' => KeyCode::KeyD,
                'E' => KeyCode::KeyE,
                'F' => KeyCode::KeyF,
                'G' => KeyCode::KeyG,
                'H' => KeyCode::KeyH,
                'I' => KeyCode::KeyI,
                'J' => KeyCode::KeyJ,
                'K' => KeyCode::KeyK,
                'L' => KeyCode::KeyL,
                'M' => KeyCode::KeyM,
                'N' => KeyCode::KeyN,
                'O' => KeyCode::KeyO,
                'P' => KeyCode::KeyP,
                'Q' => KeyCode::KeyQ,
                'R' => KeyCode::KeyR,
                'S' => KeyCode::KeyS,
                'T' => KeyCode::KeyT,
                'U' => KeyCode::KeyU,
                'V' => KeyCode::KeyV,
                'W' => KeyCode::KeyW,
                'X' => KeyCode::KeyX,
                'Y' => KeyCode::KeyY,
                'Z' => KeyCode::KeyZ,
                '0' => KeyCode::Digit0,
                '1' => KeyCode::Digit1,
                '2' => KeyCode::Digit2,
                '3' => KeyCode::Digit3,
                '4' => KeyCode::Digit4,
                '5' => KeyCode::Digit5,
                '6' => KeyCode::Digit6,
                '7' => KeyCode::Digit7,
                '8' => KeyCode::Digit8,
                '9' => KeyCode::Digit9,
                _ => return None,
            });
        }
        Some(match upper.as_str() {
            "CAPSLOCK" => KeyCode::CapsLock,
            "TAB" => KeyCode::Tab,
            "SPACE" => KeyCode::Space,
            "SHIFT" | "LEFTSHIFT" => KeyCode::ShiftLeft,
            "CTRL" | "CONTROL" | "LEFTCONTROL" => KeyCode::ControlLeft,
            "ALT" | "LEFTALT" => KeyCode::AltLeft,
            "F1" => KeyCode::F1,
            "F2" => KeyCode::F2,
            "F3" => KeyCode::F3,
            "F4" => KeyCode::F4,
            "F5" => KeyCode::F5,
            "F6" => KeyCode::F6,
            "F7" => KeyCode::F7,
            "F8" => KeyCode::F8,
            "F9" => KeyCode::F9,
            "F10" => KeyCode::F10,
            "F11" => KeyCode::F11,
            "F12" => KeyCode::F12,
            _ => return None,
        })
    };

    if let Some(p) = &game.player {
        let (yaw, pitch) = (
            f64::from(game.yaw).to_radians(),
            f64::from(game.pitch).to_radians(),
        );
        crate::game::svc_voice::set_listener(
            [
                p.position.x,
                p.position.y + f64::from(game.eye),
                p.position.z,
            ],
            [
                -yaw.sin() * pitch.cos(),
                -pitch.sin(),
                yaw.cos() * pitch.cos(),
            ],
        );
    }
    // A wall between the eyes and a speaker muffles the voice. The voice
    // thread cannot read the world, so the sight test runs here and only its
    // verdict is published.
    if let Some(p) = &game.player {
        let eye = [
            p.position.x,
            p.position.y + f64::from(game.eye),
            p.position.z,
        ];
        for (key, at) in crate::game::svc_voice::speakers() {
            let d = [at[0] - eye[0], at[1] - eye[1], at[2] - eye[2]];
            let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            let blocked = if distance < 0.5 {
                false
            } else {
                let dir = [d[0] / distance, d[1] / distance, d[2] / distance];
                game.clip_ray(eye, dir, distance).is_some()
            };
            crate::game::svc_voice::set_occluded(key, blocked);
        }
    }
    crate::game::svc_voice::on_tick();

    // Ask the server for the voice secret once we are on a server. The reply
    // arrives on the plugin channel and is picked up by the packet inject.
    if game.remote.is_some() {
        if let Some(bytes) = crate::game::svc_voice::take_secret_request() {
            game.send(horizon_protocol::packets::play::Serverbound::CustomPayload(
                horizon_protocol::packets::configuration::CustomPayload {
                    channel: "voicechat:request_secret".into(),
                    data: bytes,
                },
            ));
        }
    }

    // M toggles the microphone (§8.1). Edge-detected, so holding the key sends
    // one message per press; the payload is a single byte, 1 = disabled.
    let mute_key = key_of(&settings.mute_key).unwrap_or(KeyCode::KeyM);
    let ptt_key = key_of(&settings.ptt_key).unwrap_or(KeyCode::CapsLock);
    let gate_key = key_of(&settings.gate_key).unwrap_or(KeyCode::KeyV);
    let whisper_key = key_of(&settings.whisper_key).unwrap_or(KeyCode::AltLeft);
    let pressed = game.player.is_some() && game.remote.is_some() && game.keys.contains(&mute_key);
    if crate::game::svc_voice::mute_key_edge(pressed) {
        let disabled = crate::game::svc_voice::toggle_mute();
        log::info!("svc_voice: muted {disabled}");
        game.send(horizon_protocol::packets::play::Serverbound::CustomPayload(
            horizon_protocol::packets::configuration::CustomPayload {
                channel: "voicechat:update_state".into(),
                data: vec![u8::from(disabled)],
            },
        ));
    }

    // CapsLock is push-to-talk; V cycles voice activity, always-on and PTT.
    let in_game = game.player.is_some() && game.remote.is_some();
    crate::game::svc_voice::set_ptt(in_game && game.keys.contains(&ptt_key));
    crate::game::svc_voice::set_whisper(in_game && game.keys.contains(&whisper_key));
    if let Some(mode) = crate::game::svc_voice::cycle_gate(in_game && game.keys.contains(&gate_key)) {
        log::info!("svc_voice: gate {mode}");
    }

    // A join typed before the group list arrived: send it once the name shows
    // up. `groups_joined` clears the pending request, so this runs once.
    if in_game {
        if let Some((name, password)) = crate::game::svc_voice::take_pending_join() {
            match crate::game::svc_voice::group_by_name(&name) {
                Some(info) => match svc_engine::protocol::groups::set_group(
                    info.id,
                    password.as_deref(),
                ) {
                    Ok(payload) => {
                        log::info!("svc_voice: joining group {name} (was waiting for the list)");
                        game.send(
                            horizon_protocol::packets::play::Serverbound::CustomPayload(
                                horizon_protocol::packets::configuration::CustomPayload {
                                    channel: "voicechat:set_group".into(),
                                    data: payload,
                                },
                            ),
                        );
                    }
                    Err(e) => log::info!("svc_voice: cannot join {name}: {e:?}"),
                },
                // Not yet: put it back and try again next frame.
                None => crate::game::svc_voice::request_join(&name, password.as_deref()),
            }
        }
    }

    if let Some(c) = crate::game::svc_voice::take_counters_change() {
        log::info!("svc_voice: {c}");
    }
    if let Some(state) = crate::game::svc_voice::take_state_change() {
        log::info!("svc_voice: state {state}");
    }
    if let Some(e) = crate::game::svc_voice::take_error() {
        log::warn!("svc_voice: {e}");
    }

    // Once the secret is in, open the UDP session on its own thread.
    let address = game.remote.as_ref().map(|r| r.session.address.clone());
    if let Some(address) = address {
        if let Some(endpoint) = crate::game::svc_voice::ensure_started(&address) {
            log::info!("svc_voice: voice session to {endpoint}");
        }
    }
}
