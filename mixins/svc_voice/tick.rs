// Runs at the head of the client's per-frame world update.
//
// The binding gives the whole `Game`, so the listener's eye position and look
// direction are published here without the voice thread ever touching the game.
// `Game` carries `player: Option<PlayerMotion>` (position), plus `yaw`, `pitch`
// and `eye` (eye height); the look vector is the same one `view_vector` in
// game.rs builds.
fn svc_voice_tick(game: &mut crate::game::Game<'_>) {
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
    let pressed = game.player.is_some() && game.remote.is_some() && game.keys.contains(&KeyCode::KeyM);
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
