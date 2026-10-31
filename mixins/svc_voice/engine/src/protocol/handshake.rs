//! Discovery handshake over the Minecraft custom-payload channel (SPEC §2).
//!
//! Only the payload *bodies* are produced/parsed here — the Minecraft framing
//! (packet id, channel name, compression) is added by the client, because SVC
//! writes the structure straight into the payload buffer with no extra header
//! (§2.1).
//!
//! The other `voicechat:*` payloads of §8 (player states, groups, categories)
//! are listed as channel-name constants but not modelled yet: Phase 1 only
//! needs the handshake, and audio does not depend on those payloads.

use std::fmt;

use crate::protocol::wire::{Reader, Uuid, WireError, Writer};

/// Compatibility version of the analysed revision (§2.2, §10).
pub const COMPATIBILITY_VERSION: i32 = 20;

// Channel names (§2.1). The channel string *is* the payload-type identifier.
pub const CHANNEL_REQUEST_SECRET: &str = "voicechat:request_secret";
pub const CHANNEL_SECRET: &str = "voicechat:secret";
pub const CHANNEL_UPDATE_STATE: &str = "voicechat:update_state";
pub const CHANNEL_STATE: &str = "voicechat:state";
pub const CHANNEL_STATES: &str = "voicechat:states";
pub const CHANNEL_REMOVE_STATE: &str = "voicechat:remove_state";
pub const CHANNEL_ADD_GROUP: &str = "voicechat:add_group";
pub const CHANNEL_REMOVE_GROUP: &str = "voicechat:remove_group";
pub const CHANNEL_SET_GROUP: &str = "voicechat:set_group";
pub const CHANNEL_CREATE_GROUP: &str = "voicechat:create_group";
pub const CHANNEL_LEAVE_GROUP: &str = "voicechat:leave_group";
pub const CHANNEL_JOINED_GROUP: &str = "voicechat:joined_group";
pub const CHANNEL_ADD_CATEGORY: &str = "voicechat:add_category";
pub const CHANNEL_REMOVE_CATEGORY: &str = "voicechat:remove_category";

/// Legacy 1.12 proxy channels (§2.1) — not needed by a modern client.
pub const CHANNEL_SECRET_LEGACY: &str = "vc:secret";
pub const CHANNEL_REQUEST_SECRET_LEGACY: &str = "vc:request_secret";

/// `voicechat:secret.voiceHost` is a `String` without a documented cap; Java
/// writes it with the default 32767-character limit.
pub const MAX_VOICE_HOST_CHARS: usize = 32767;

/// Encoder tuning selected by the `codec` byte (§2.3, §7.1). A decoder needs
/// no knowledge of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Voip,
    Audio,
    RestrictedLowDelay,
    /// Ordinal outside 0..=2. Kept verbatim rather than rejected so that a
    /// future server value does not break the handshake.
    Unknown(u8),
}

impl Codec {
    pub fn from_ordinal(v: u8) -> Self {
        match v {
            0 => Codec::Voip,
            1 => Codec::Audio,
            2 => Codec::RestrictedLowDelay,
            other => Codec::Unknown(other),
        }
    }

    pub fn ordinal(self) -> u8 {
        match self {
            Codec::Voip => 0,
            Codec::Audio => 1,
            Codec::RestrictedLowDelay => 2,
            Codec::Unknown(v) => v,
        }
    }
}

/// `voicechat:request_secret` (C→S), §2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestSecret {
    pub compatibility_version: i32,
}

impl Default for RequestSecret {
    fn default() -> Self {
        RequestSecret {
            compatibility_version: COMPATIBILITY_VERSION,
        }
    }
}

impl RequestSecret {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(4);
        w.write_i32(self.compatibility_version);
        w.into_inner()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(bytes);
        let compatibility_version = r.read_i32()?;
        r.ensure_end()?;
        Ok(RequestSecret {
            compatibility_version,
        })
    }
}

/// `voicechat:secret` (S→C), §2.3. Field order is authoritative.
#[derive(Debug, Clone, PartialEq)]
pub struct SecretPayload {
    /// AES-128 key for the UDP channel, 16 bytes, no length prefix.
    pub secret: [u8; 16],
    /// UDP port the voice server is bound to.
    pub server_port: i32,
    /// The receiving player's own UUID (used in every outgoing datagram header).
    pub player_uuid: Uuid,
    pub codec: Codec,
    /// Max Opus payload the server accepts (default 1275, range 512..=2048).
    pub mtu_size: i32,
    /// Speaking distance in blocks.
    pub voice_chat_distance: f64,
    /// Keep-alive period in ms (default 1000).
    pub keep_alive: i32,
    pub groups_enabled: bool,
    /// Overrides host and/or port; may be empty (§2.3.1).
    pub voice_host: String,
    pub allow_recording: bool,
}

impl SecretPayload {
    pub fn encode(&self) -> Result<Vec<u8>, HandshakeError> {
        let mut w = Writer::with_capacity(64);
        w.write_fixed_secret(&self.secret);
        w.write_i32(self.server_port);
        w.write_uuid(self.player_uuid);
        w.write_u8(self.codec.ordinal());
        w.write_i32(self.mtu_size);
        w.write_f64(self.voice_chat_distance);
        w.write_i32(self.keep_alive);
        w.write_bool(self.groups_enabled);
        w.write_string(&self.voice_host, MAX_VOICE_HOST_CHARS)?;
        w.write_bool(self.allow_recording);
        Ok(w.into_inner())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(bytes);
        let secret = r.read_fixed_secret()?;
        let server_port = r.read_i32()?;
        let player_uuid = r.read_uuid()?;
        let codec = Codec::from_ordinal(r.read_u8()?);
        let mtu_size = r.read_i32()?;
        let voice_chat_distance = r.read_f64()?;
        let keep_alive = r.read_i32()?;
        let groups_enabled = r.read_bool()?;
        let voice_host = r.read_string(MAX_VOICE_HOST_CHARS)?;
        let allow_recording = r.read_bool()?;
        r.ensure_end()?;
        Ok(SecretPayload {
            secret,
            server_port,
            player_uuid,
            codec,
            mtu_size,
            voice_chat_distance,
            keep_alive,
            groups_enabled,
            voice_host,
            allow_recording,
        })
    }

    /// Keep-alive period as a `Duration`, guarding against nonsense values.
    pub fn keep_alive_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.keep_alive.max(1) as u64)
    }

    /// Effective keep-alive interval, honouring the server's documented minimum
    /// of 1000 ms (§6.2 "minimum 1000").
    pub fn effective_keep_alive(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.keep_alive.max(1000) as u64)
    }
}

// ---------------------------------------------------------------------------
// Voice endpoint resolution (§2.3.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceEndpoint {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceHostError {
    /// A numeric `voiceHost` that is not a usable port.
    PortOutOfRange(u32),
    /// Empty host part, e.g. `":24454"`.
    EmptyHost,
    /// Unbracketed IPv6 literal — the rule set of §2.3.1 does not cover it
    /// (OPEN_QUESTIONS #2).
    AmbiguousHost(String),
    /// Bracketed host without a closing bracket, or junk after it.
    Malformed(String),
}

impl fmt::Display for VoiceHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VoiceHostError::PortOutOfRange(p) => write!(f, "port {p} is out of range"),
            VoiceHostError::EmptyHost => write!(f, "empty host"),
            VoiceHostError::AmbiguousHost(h) => {
                write!(f, "ambiguous unbracketed IPv6 voice_host {h:?}")
            }
            VoiceHostError::Malformed(h) => write!(f, "malformed voice_host {h:?}"),
        }
    }
}

impl std::error::Error for VoiceHostError {}

fn parse_port(s: &str) -> Result<u16, VoiceHostError> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return Err(VoiceHostError::Malformed(s.to_string()));
    }
    let value: u32 = s
        .parse()
        .map_err(|_| VoiceHostError::Malformed(s.to_string()))?;
    if value == 0 || value > 65535 {
        return Err(VoiceHostError::PortOutOfRange(value));
    }
    Ok(value as u16)
}

/// Apply the `voiceHost` rules of §2.3.1 to the address the Minecraft
/// connection used.
///
/// `SPEC-AMBIGUITY`: the spec describes this as a URI parse with an added
/// `voicechat://` scheme. A host that contains several colons but is not
/// bracketed cannot be disambiguated that way, so it is rejected as
/// [`VoiceHostError::AmbiguousHost`] rather than guessed.
pub fn resolve_voice_endpoint(
    server_host: &str,
    server_port: u16,
    voice_host: &str,
) -> Result<VoiceEndpoint, VoiceHostError> {
    let vh = voice_host.trim();
    if vh.is_empty() {
        return Ok(VoiceEndpoint {
            host: server_host.to_string(),
            port: server_port,
        });
    }
    if vh == "rtc-local" || vh == "rtc-remote" {
        // Legacy P2P modes; irrelevant for a normal server (§2.3.1).
        return Ok(VoiceEndpoint {
            host: "127.0.0.1".to_string(),
            port: server_port,
        });
    }
    if vh.chars().all(|c| c.is_ascii_digit()) {
        let value: u32 = vh
            .parse()
            .map_err(|_| VoiceHostError::PortOutOfRange(u32::MAX))?;
        if value == 0 || value > 65535 {
            return Err(VoiceHostError::PortOutOfRange(value));
        }
        return Ok(VoiceEndpoint {
            host: server_host.to_string(),
            port: value as u16,
        });
    }
    if let Some(rest) = vh.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| VoiceHostError::Malformed(vh.to_string()))?;
        if host.is_empty() {
            return Err(VoiceHostError::EmptyHost);
        }
        if tail.is_empty() {
            return Ok(VoiceEndpoint {
                host: host.to_string(),
                port: server_port,
            });
        }
        let port_str = tail
            .strip_prefix(':')
            .ok_or_else(|| VoiceHostError::Malformed(vh.to_string()))?;
        return Ok(VoiceEndpoint {
            host: host.to_string(),
            port: parse_port(port_str)?,
        });
    }
    if vh.matches(':').count() > 1 {
        return Err(VoiceHostError::AmbiguousHost(vh.to_string()));
    }
    if let Some((host, port_str)) = vh.rsplit_once(':') {
        if host.is_empty() {
            return Err(VoiceHostError::EmptyHost);
        }
        return Ok(VoiceEndpoint {
            host: host.to_string(),
            port: parse_port(port_str)?,
        });
    }
    Ok(VoiceEndpoint {
        host: vh.to_string(),
        port: server_port,
    })
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    Wire(WireError),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandshakeError::Wire(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<WireError> for HandshakeError {
    fn from(e: WireError) -> Self {
        HandshakeError::Wire(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SecretPayload {
        SecretPayload {
            secret: [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF,
            ],
            server_port: 24454,
            player_uuid: Uuid::parse_str("58bc9ae9-c7a8-45e4-a11c-efbb67199425").unwrap(),
            codec: Codec::Voip,
            mtu_size: 1275,
            voice_chat_distance: 48.0,
            keep_alive: 1000,
            groups_enabled: true,
            voice_host: String::new(),
            allow_recording: false,
        }
    }

    #[test]
    fn request_secret_known_bytes() {
        assert_eq!(RequestSecret::new().encode(), vec![0x00, 0x00, 0x00, 0x14]);
        assert_eq!(
            RequestSecret::decode(&[0x00, 0x00, 0x00, 0x14]),
            Ok(RequestSecret {
                compatibility_version: 20
            })
        );
        assert_eq!(COMPATIBILITY_VERSION, 20);
    }

    #[test]
    fn request_secret_rejects_short_payload() {
        assert!(RequestSecret::decode(&[0, 0, 0]).is_err());
        assert!(RequestSecret::decode(&[]).is_err());
    }

    #[test]
    fn request_secret_rejects_trailing_bytes() {
        assert_eq!(
            RequestSecret::decode(&[0, 0, 0, 20, 0]),
            Err(HandshakeError::Wire(WireError::TrailingBytes { count: 1 }))
        );
    }

    #[test]
    fn secret_payload_exact_field_order() {
        let bytes = sample().encode().unwrap();
        // 16 + 4 + 16 + 1 + 4 + 8 + 4 + 1 + 1(empty string) + 1 = 56
        assert_eq!(bytes.len(), 56);
        assert_eq!(&bytes[0..16], &sample().secret);
        assert_eq!(&bytes[16..20], &24454i32.to_be_bytes());
        assert_eq!(&bytes[20..36], &sample().player_uuid.to_bytes());
        assert_eq!(bytes[36], 0); // codec VOIP
        assert_eq!(&bytes[37..41], &1275i32.to_be_bytes());
        assert_eq!(&bytes[41..49], &48.0f64.to_be_bytes());
        assert_eq!(&bytes[49..53], &1000i32.to_be_bytes());
        assert_eq!(bytes[53], 1); // groups enabled
        assert_eq!(bytes[54], 0); // empty voice host (VarInt length 0)
        assert_eq!(bytes[55], 0); // recording not allowed
    }

    #[test]
    fn secret_payload_roundtrip_with_concrete_values() {
        let p = sample();
        assert_eq!(SecretPayload::decode(&p.encode().unwrap()), Ok(p));
    }

    #[test]
    fn secret_payload_roundtrip_with_host_and_flags() {
        let mut p = sample();
        p.voice_host = "voice.example.com:24455".to_string();
        p.allow_recording = true;
        p.groups_enabled = false;
        p.codec = Codec::RestrictedLowDelay;
        p.server_port = 24455;
        let decoded = SecretPayload::decode(&p.encode().unwrap()).unwrap();
        assert_eq!(decoded, p);
        assert_eq!(decoded.voice_host, "voice.example.com:24455");
        assert_eq!(decoded.codec.ordinal(), 2);
    }

    #[test]
    fn secret_payload_rejects_truncation_at_every_offset() {
        let bytes = sample().encode().unwrap();
        for cut in 0..bytes.len() {
            assert!(
                SecretPayload::decode(&bytes[..cut]).is_err(),
                "prefix of {cut} bytes decoded"
            );
        }
    }

    #[test]
    fn secret_payload_rejects_extra_bytes() {
        let mut bytes = sample().encode().unwrap();
        bytes.push(0xAA);
        assert!(SecretPayload::decode(&bytes).is_err());
    }

    #[test]
    fn unknown_codec_is_preserved_not_rejected() {
        assert_eq!(Codec::from_ordinal(7), Codec::Unknown(7));
        let mut p = sample();
        p.codec = Codec::Unknown(7);
        let decoded = SecretPayload::decode(&p.encode().unwrap()).unwrap();
        assert_eq!(decoded.codec, Codec::Unknown(7));
    }

    #[test]
    fn keep_alive_has_a_floor_of_one_second() {
        let mut p = sample();
        p.keep_alive = 200;
        assert_eq!(p.effective_keep_alive().as_millis(), 1000);
        p.keep_alive = 2500;
        assert_eq!(p.effective_keep_alive().as_millis(), 2500);
        p.keep_alive = 0;
        assert_eq!(p.keep_alive_interval().as_millis(), 1);
    }

    #[test]
    fn channel_names_follow_the_voicechat_namespace() {
        for ch in [
            CHANNEL_REQUEST_SECRET,
            CHANNEL_SECRET,
            CHANNEL_UPDATE_STATE,
            CHANNEL_STATE,
            CHANNEL_STATES,
            CHANNEL_REMOVE_STATE,
            CHANNEL_ADD_GROUP,
            CHANNEL_REMOVE_GROUP,
            CHANNEL_SET_GROUP,
            CHANNEL_CREATE_GROUP,
            CHANNEL_LEAVE_GROUP,
            CHANNEL_JOINED_GROUP,
            CHANNEL_ADD_CATEGORY,
            CHANNEL_REMOVE_CATEGORY,
        ] {
            assert!(ch.starts_with("voicechat:"), "{ch}");
        }
        assert_eq!(CHANNEL_SECRET_LEGACY, "vc:secret");
    }

    #[test]
    fn voice_host_empty_uses_server_address() {
        let ep = resolve_voice_endpoint("mc.example.com", 24454, "").unwrap();
        assert_eq!(ep.host, "mc.example.com");
        assert_eq!(ep.port, 24454);
    }

    #[test]
    fn voice_host_numeric_overrides_only_the_port() {
        let ep = resolve_voice_endpoint("mc.example.com", 24454, "24455").unwrap();
        assert_eq!(ep.host, "mc.example.com");
        assert_eq!(ep.port, 24455);
    }

    #[test]
    fn voice_host_host_and_port_forms() {
        let ep = resolve_voice_endpoint("mc.example.com", 24454, "voice.example.com").unwrap();
        assert_eq!(ep, VoiceEndpoint { host: "voice.example.com".into(), port: 24454 });

        let ep = resolve_voice_endpoint("mc.example.com", 24454, "voice.example.com:1234").unwrap();
        assert_eq!(ep, VoiceEndpoint { host: "voice.example.com".into(), port: 1234 });
    }

    #[test]
    fn voice_host_bracketed_ipv6() {
        let ep = resolve_voice_endpoint("mc.example.com", 24454, "[::1]:24455").unwrap();
        assert_eq!(ep, VoiceEndpoint { host: "::1".into(), port: 24455 });
        let ep = resolve_voice_endpoint("mc.example.com", 24454, "[::1]").unwrap();
        assert_eq!(ep, VoiceEndpoint { host: "::1".into(), port: 24454 });
    }

    #[test]
    fn voice_host_rtc_modes_use_loopback() {
        for vh in ["rtc-local", "rtc-remote"] {
            let ep = resolve_voice_endpoint("mc.example.com", 24454, vh).unwrap();
            assert_eq!(ep, VoiceEndpoint { host: "127.0.0.1".into(), port: 24454 });
        }
    }

    #[test]
    fn voice_host_errors() {
        assert_eq!(
            resolve_voice_endpoint("h", 1, "0"),
            Err(VoiceHostError::PortOutOfRange(0))
        );
        assert_eq!(
            resolve_voice_endpoint("h", 1, "70000"),
            Err(VoiceHostError::PortOutOfRange(70000))
        );
        assert_eq!(
            resolve_voice_endpoint("h", 1, ":24454"),
            Err(VoiceHostError::EmptyHost)
        );
        assert!(matches!(
            resolve_voice_endpoint("h", 1, "::1"),
            Err(VoiceHostError::AmbiguousHost(_))
        ));
        assert!(matches!(
            resolve_voice_endpoint("h", 1, "[::1"),
            Err(VoiceHostError::Malformed(_))
        ));
    }

    #[test]
    fn numeric_looking_hostnames_are_not_ports() {
        // "24454.example.com" is a host, not a port (§2.3.1, OPEN_QUESTIONS #2).
        let ep = resolve_voice_endpoint("mc", 24454, "24454.example.com").unwrap();
        assert_eq!(ep, VoiceEndpoint { host: "24454.example.com".into(), port: 24454 });
    }
}
