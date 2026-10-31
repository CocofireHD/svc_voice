// Runs at the head of the client's serverbound-packet handler, where every
// serverbound/plugin message the server sends is decoded. Only the voice chat
// channels and the entity bookkeeping for positional audio are touched;
// everything else falls through to the game unchanged.
fn svc_voice_packet(
    game: &mut crate::game::Game<'_>,
    packet: &horizon_protocol::packets::play::Clientbound,
) {
    use horizon_protocol::packets::play::Clientbound as C;
    let _ = game;
    match packet {
        C::CustomPayload(p) => {
            crate::game::svc_voice::on_plugin_message(&p.channel, &p.data);
        }
        // §7.4: the speaker's position comes from the entity, keyed by the UUID
        // the voice packet carries. `AddEntity` is where the two meet.
        // The settings file names players, the voice packets carry UUIDs.
        C::PlayerInfoUpdate(p) => {
            for entry in &p.entries {
                if let Some(name) = &entry.name {
                    crate::game::svc_voice::set_player_name(entry.uuid.to_be_bytes(), name);
                }
            }
        }
        C::AddEntity(p) => {
            let key = p.uuid.to_be_bytes();
            crate::game::svc_voice::map_entity(p.id, key);
            crate::game::svc_voice::set_entity(key, p.position);
        }
        C::TeleportEntity(p) => {
            if let Some(key) = crate::game::svc_voice::entity_key(p.id) {
                crate::game::svc_voice::set_entity(key, p.position);
            }
        }
        C::EntityPositionSync(p) => {
            if let Some(key) = crate::game::svc_voice::entity_key(p.id) {
                crate::game::svc_voice::set_entity(key, p.path.end());
            }
        }
        C::RemoveEntities(p) => {
            for id in &p.ids {
                if let Some(key) = crate::game::svc_voice::forget_entity(*id) {
                    crate::game::svc_voice::remove_entity(key);
                }
            }
        }
        _ => {}
    }
}
