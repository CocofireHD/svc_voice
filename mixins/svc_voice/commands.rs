// Client commands for the voice chat, `/vc <what>`.
//
// This hook runs before the server and Horizon's own commands see the line, so
// `/vc join Team pw` never leaves the client as a chat message. Setting `ran`
// is what stops the normal path.
//
//   /vc create <name> [password]        a normal group
//   /vc create open|isolated <name> [password]
//   /vc join <name> [password]
//   /vc leave
//   /vc test [seconds]                  play the microphone back locally
//   /vc speaker [seconds]               play a 440 Hz tone
//   /vc list
//   /vc who
fn svc_voice_commands(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    let line = command.trim();
    let Some(rest) = line.strip_prefix("vc") else {
        return;
    };
    // `vchat`, `vcc` and similar are somebody else's command.
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return;
    }
    let mut words = rest.split_whitespace();
    let action = words.next().unwrap_or("list").to_ascii_lowercase();

    // Only a live session can carry these payloads.
    if game.remote.is_none() {
        log::info!("svc_voice: /vc needs a server connection");
        *ran = true;
        return;
    }

    let send = |game: &mut crate::game::Game<'_>, channel: &str, data: Vec<u8>| {
        game.send(horizon_protocol::packets::play::Serverbound::CustomPayload(
            horizon_protocol::packets::configuration::CustomPayload {
                channel: channel.to_string(),
                data,
            },
        ));
    };

    match action.as_str() {
        "create" => {
            // An optional kind before the name, then the name, then a password.
            let mut kind = svc_engine::protocol::groups::GroupKind::Normal;
            let mut name = words.next();
            match name.map(str::to_ascii_lowercase).as_deref() {
                Some("open") => {
                    kind = svc_engine::protocol::groups::GroupKind::Open;
                    name = words.next();
                }
                Some("isolated") => {
                    kind = svc_engine::protocol::groups::GroupKind::Isolated;
                    name = words.next();
                }
                _ => {}
            }
            let Some(name) = name else {
                log::info!("svc_voice: usage: /vc create [open|isolated] <name> [password]");
                *ran = true;
                return;
            };
            let password = words.next();
            match svc_engine::protocol::groups::create_group(name, password, kind) {
                Ok(payload) => {
                    send(game, "voicechat:create_group", payload);
                    log::info!("svc_voice: creating {} group {name}", kind.name());
                }
                Err(e) => log::info!("svc_voice: bad group name or password: {e:?}"),
            }
            *ran = true;
        }
        "join" => {
            let Some(name) = words.next() else {
                log::info!("svc_voice: usage: /vc join <name> [password]");
                *ran = true;
                return;
            };
            let password = words.next();
            match crate::game::svc_voice::group_by_name(name) {
                Some(info) => match svc_engine::protocol::groups::set_group(info.id, password) {
                    Ok(payload) => {
                        send(game, "voicechat:set_group", payload);
                        log::info!("svc_voice: joining group {name}");
                    }
                    Err(e) => log::info!("svc_voice: cannot join {name}: {e:?}"),
                },
                None => {
                    // The server sends its group list once its compatibility
                    // check passes, which can be after this line is typed.
                    crate::game::svc_voice::request_join(name, password);
                    log::info!(
                        "svc_voice: no group called {name} yet, joining when it arrives; known: {}",
                        crate::game::svc_voice::group_summary()
                    );
                }
            }
            *ran = true;
        }
        "leave" => {
            send(
                game,
                "voicechat:leave_group",
                svc_engine::protocol::groups::leave_group(),
            );
            log::info!("svc_voice: leaving the group");
            *ran = true;
        }
        "test" => {
            let seconds = words
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5);
            crate::game::svc_voice::start_mic_test(seconds);
            *ran = true;
        }
        "speaker" => {
            let seconds = words.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(3);
            crate::game::svc_voice::start_speaker_test(seconds);
            *ran = true;
        }
        "list" | "who" => {
            log::info!(
                "svc_voice: groups: {}",
                crate::game::svc_voice::group_summary()
            );
            *ran = true;
        }
        _ => {
            log::info!(
                "svc_voice: /vc create|join|leave|test|list — groups: {}",
                crate::game::svc_voice::group_summary()
            );
            *ran = true;
        }
    }
}
