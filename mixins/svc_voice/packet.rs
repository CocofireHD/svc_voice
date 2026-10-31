// Runs at the head of the client's serverbound-packet handler, where every
// serverbound/plugin message the server sends is decoded. Only the voice chat
// channels are touched; everything else falls through to the game unchanged.
fn svc_voice_packet(
    game: &mut crate::game::Game<'_>,
    packet: &horizon_protocol::packets::play::Clientbound,
) {
    let _ = game;
    if let horizon_protocol::packets::play::Clientbound::CustomPayload(p) = packet {
        crate::game::svc_voice::on_plugin_message(&p.channel, &p.data);
    }
}
