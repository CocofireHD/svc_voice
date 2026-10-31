//! The ten UDP packet types, ids `0x01`..=`0x0A` (SPEC §5).
//!
//! Every packet starts with a one-byte id **inside the ciphertext**; the field
//! order below is the authoritative order from the spec tables.
//!
//! Notable details that are easy to get wrong:
//! * `MicPacket` carries a `long` sequence number and a trailing boolean;
//!   a zero-length `data` field is the legal "I stopped talking" marker (§5.1).
//! * `PlayerSoundPacket` has a `float distance` **before** the flags byte (§5.2),
//!   `LocationSoundPacket` has `x,y,z` **before** `data` (§5.4).
//! * `PingPacket.id` is a 16-byte UUID, not an integer (§5.7).
//! * the optional `category` string is gated by bit 1 of the flags byte; if the
//!   bit is clear the string is absent, so a decoder that always reads it
//!   desynchronises every following packet.

use std::fmt;

use crate::protocol::wire::{Reader, Uuid, WireError, Writer};

/// `0x01` MicPacket (C→S).
pub const ID_MIC: u8 = 0x01;
/// `0x02` PlayerSoundPacket (S→C).
pub const ID_PLAYER_SOUND: u8 = 0x02;
/// `0x03` GroupSoundPacket (S→C).
pub const ID_GROUP_SOUND: u8 = 0x03;
/// `0x04` LocationSoundPacket (S→C).
pub const ID_LOCATION_SOUND: u8 = 0x04;
/// `0x05` AuthenticatePacket (C→S).
pub const ID_AUTHENTICATE: u8 = 0x05;
/// `0x06` AuthenticateAckPacket (S→C).
pub const ID_AUTHENTICATE_ACK: u8 = 0x06;
/// `0x07` PingPacket (both).
pub const ID_PING: u8 = 0x07;
/// `0x08` KeepAlivePacket (both).
pub const ID_KEEP_ALIVE: u8 = 0x08;
/// `0x09` ConnectionCheckPacket (C→S).
pub const ID_CONNECTION_CHECK: u8 = 0x09;
/// `0x0A` ConnectionCheckAckPacket (S→C).
pub const ID_CONNECTION_CHECK_ACK: u8 = 0x0A;

/// Maximum Opus payload on the wire (§3.3, §7.1).
pub const MAX_OPUS_PAYLOAD: usize = 1275;
/// Maximum length of the optional sound `category` string in characters (§5.2).
pub const MAX_SOUND_CATEGORY_CHARS: usize = 16;

/// Flags byte of the sound packets.
pub const WHISPER_MASK: u8 = 0b0000_0001;
/// Bit 1: an optional category string follows.
pub const HAS_CATEGORY_MASK: u8 = 0b0000_0010;

/// TTL of a `MicPacket` in the server queue (§5, §6.1).
pub const MIC_TTL_MS: u64 = 500;
/// Default TTL of every other packet (§5).
pub const DEFAULT_TTL_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketError {
    Wire(WireError),
    /// Packet id outside `0x01`..=`0x0A`.
    UnknownId(u8),
    /// Opus payload larger than 1275 bytes.
    OpusPayloadTooLarge { len: usize },
    /// Bytes left over after the declared structure.
    TrailingBytes { count: usize },
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PacketError::Wire(e) => write!(f, "{e}"),
            PacketError::UnknownId(id) => write!(f, "unknown packet id 0x{id:02x}"),
            PacketError::OpusPayloadTooLarge { len } => {
                write!(f, "opus payload of {len} bytes exceeds {MAX_OPUS_PAYLOAD}")
            }
            PacketError::TrailingBytes { count } => write!(f, "{count} trailing bytes after packet"),
        }
    }
}

impl std::error::Error for PacketError {}

impl From<WireError> for PacketError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::TrailingBytes { count } => PacketError::TrailingBytes { count },
            other => PacketError::Wire(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Packet structs
// ---------------------------------------------------------------------------

/// `0x01` — one encoded Opus frame from the speaking client (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mic {
    pub data: Vec<u8>,
    pub sequence: i64,
    pub whispering: bool,
}

impl Mic {
    /// The mandatory "end of transmission" frame: empty payload, not whisper.
    pub fn stop(sequence: i64) -> Self {
        Mic {
            data: Vec::new(),
            sequence,
            whispering: false,
        }
    }

    pub fn is_stop(&self) -> bool {
        self.data.is_empty()
    }
}

/// `0x02` — proximity audio attributed to a player (§5.2).
///
/// `SPEC-AMBIGUITY`: the brief's field list omitted `distance`; SPEC §5.2 places
/// a `float distance` between `sequenceNumber` and `flags`, and §7.4 makes it
/// authoritative for the audible radius.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSound {
    pub channel_id: Uuid,
    pub sender: Uuid,
    pub data: Vec<u8>,
    pub sequence: i64,
    /// Audible radius for this frame, in blocks (server default 48.0, whisper 24.0).
    pub distance: f32,
    pub whispering: bool,
    pub category: Option<String>,
}

/// `0x03` — group audio, played without positional attenuation (§5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSound {
    pub channel_id: Uuid,
    pub sender: Uuid,
    pub data: Vec<u8>,
    pub sequence: i64,
    pub category: Option<String>,
}

/// `0x04` — audio at explicit world coordinates (§5.4).
///
/// `SPEC-AMBIGUITY`: field order is `channelId, sender, x, y, z, data,
/// sequence, distance, flags, [category]` — the coordinates come *before* the
/// Opus payload, unlike every other sound packet.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationSound {
    pub channel_id: Uuid,
    pub sender: Uuid,
    pub position: [f64; 3],
    pub data: Vec<u8>,
    pub sequence: i64,
    pub distance: f32,
    pub category: Option<String>,
}

/// `0x05` — binds the source address to the secret (§5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticate {
    pub player: Uuid,
    pub secret: [u8; 16],
}

/// `0x07` — RTT probe; the client must echo the identical fields (§5.7).
///
/// `SPEC-AMBIGUITY`: SPEC §5.7 types `id` as a 16-byte `UUID` (and §5.9 uses a
/// random UUID for the external ping), while the milestone brief said `i64 id`.
/// The spec wins: `UUID(16) ‖ i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    pub id: Uuid,
    pub timestamp: i64,
}

/// Any of the ten UDP packets.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Mic(Mic),
    PlayerSound(PlayerSound),
    GroupSound(GroupSound),
    LocationSound(LocationSound),
    Authenticate(Authenticate),
    AuthenticateAck,
    Ping(Ping),
    KeepAlive,
    ConnectionCheck,
    ConnectionCheckAck,
}

impl Packet {
    pub fn id(&self) -> u8 {
        match self {
            Packet::Mic(_) => ID_MIC,
            Packet::PlayerSound(_) => ID_PLAYER_SOUND,
            Packet::GroupSound(_) => ID_GROUP_SOUND,
            Packet::LocationSound(_) => ID_LOCATION_SOUND,
            Packet::Authenticate(_) => ID_AUTHENTICATE,
            Packet::AuthenticateAck => ID_AUTHENTICATE_ACK,
            Packet::Ping(_) => ID_PING,
            Packet::KeepAlive => ID_KEEP_ALIVE,
            Packet::ConnectionCheck => ID_CONNECTION_CHECK,
            Packet::ConnectionCheckAck => ID_CONNECTION_CHECK_ACK,
        }
    }

    /// Time-to-live in the receiver's queue (§5): 500 ms for mic audio,
    /// 10 000 ms for everything else.
    pub fn ttl_ms(&self) -> u64 {
        match self {
            Packet::Mic(_) => MIC_TTL_MS,
            _ => DEFAULT_TTL_MS,
        }
    }

    /// Serialise `id ‖ fields` (the plaintext that gets encrypted).
    pub fn encode(&self) -> Result<Vec<u8>, PacketError> {
        let mut w = Writer::with_capacity(64);
        match self {
            Packet::Mic(p) => {
                w.write_u8(ID_MIC);
                w.write_byte_array(&p.data, MAX_OPUS_PAYLOAD)?;
                w.write_i64(p.sequence);
                w.write_bool(p.whispering);
            }
            Packet::PlayerSound(p) => {
                w.write_u8(ID_PLAYER_SOUND);
                w.write_uuid(p.channel_id);
                w.write_uuid(p.sender);
                w.write_byte_array(&p.data, MAX_OPUS_PAYLOAD)?;
                w.write_i64(p.sequence);
                w.write_f32(p.distance);
                let mut flags = 0u8;
                if p.whispering {
                    flags |= WHISPER_MASK;
                }
                if p.category.is_some() {
                    flags |= HAS_CATEGORY_MASK;
                }
                w.write_u8(flags);
                if let Some(cat) = &p.category {
                    w.write_string(cat, MAX_SOUND_CATEGORY_CHARS)?;
                }
            }
            Packet::GroupSound(p) => {
                w.write_u8(ID_GROUP_SOUND);
                w.write_uuid(p.channel_id);
                w.write_uuid(p.sender);
                w.write_byte_array(&p.data, MAX_OPUS_PAYLOAD)?;
                w.write_i64(p.sequence);
                // Whisper is never used for group audio (§5.3): only bit 1.
                w.write_u8(if p.category.is_some() {
                    HAS_CATEGORY_MASK
                } else {
                    0
                });
                if let Some(cat) = &p.category {
                    w.write_string(cat, MAX_SOUND_CATEGORY_CHARS)?;
                }
            }
            Packet::LocationSound(p) => {
                w.write_u8(ID_LOCATION_SOUND);
                w.write_uuid(p.channel_id);
                w.write_uuid(p.sender);
                // Coordinates come *before* the audio payload (§5.4).
                w.write_f64(p.position[0]);
                w.write_f64(p.position[1]);
                w.write_f64(p.position[2]);
                w.write_byte_array(&p.data, MAX_OPUS_PAYLOAD)?;
                w.write_i64(p.sequence);
                w.write_f32(p.distance);
                w.write_u8(if p.category.is_some() {
                    HAS_CATEGORY_MASK
                } else {
                    0
                });
                if let Some(cat) = &p.category {
                    w.write_string(cat, MAX_SOUND_CATEGORY_CHARS)?;
                }
            }
            Packet::Authenticate(p) => {
                w.write_u8(ID_AUTHENTICATE);
                w.write_uuid(p.player);
                w.write_fixed_secret(&p.secret);
            }
            Packet::AuthenticateAck => w.write_u8(ID_AUTHENTICATE_ACK),
            Packet::Ping(p) => {
                w.write_u8(ID_PING);
                w.write_uuid(p.id);
                w.write_i64(p.timestamp);
            }
            Packet::KeepAlive => w.write_u8(ID_KEEP_ALIVE),
            Packet::ConnectionCheck => w.write_u8(ID_CONNECTION_CHECK),
            Packet::ConnectionCheckAck => w.write_u8(ID_CONNECTION_CHECK_ACK),
        }
        Ok(w.into_inner())
    }

    /// Parse `id ‖ fields`; rejects unknown ids and trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Packet, PacketError> {
        let mut r = Reader::new(bytes);
        let id = r.read_u8()?;
        let packet = match id {
            ID_MIC => {
                let data = r.read_byte_array(MAX_OPUS_PAYLOAD)?.to_vec();
                let sequence = r.read_i64()?;
                let whispering = r.read_bool()?;
                Packet::Mic(Mic {
                    data,
                    sequence,
                    whispering,
                })
            }
            ID_PLAYER_SOUND => {
                let channel_id = r.read_uuid()?;
                let sender = r.read_uuid()?;
                let data = r.read_byte_array(MAX_OPUS_PAYLOAD)?.to_vec();
                let sequence = r.read_i64()?;
                let distance = r.read_f32()?;
                let flags = r.read_u8()?;
                let category = read_category(&mut r, flags)?;
                Packet::PlayerSound(PlayerSound {
                    channel_id,
                    sender,
                    data,
                    sequence,
                    distance,
                    whispering: flags & WHISPER_MASK != 0,
                    category,
                })
            }
            ID_GROUP_SOUND => {
                let channel_id = r.read_uuid()?;
                let sender = r.read_uuid()?;
                let data = r.read_byte_array(MAX_OPUS_PAYLOAD)?.to_vec();
                let sequence = r.read_i64()?;
                let flags = r.read_u8()?;
                let category = read_category(&mut r, flags)?;
                Packet::GroupSound(GroupSound {
                    channel_id,
                    sender,
                    data,
                    sequence,
                    category,
                })
            }
            ID_LOCATION_SOUND => {
                let channel_id = r.read_uuid()?;
                let sender = r.read_uuid()?;
                let x = r.read_f64()?;
                let y = r.read_f64()?;
                let z = r.read_f64()?;
                let data = r.read_byte_array(MAX_OPUS_PAYLOAD)?.to_vec();
                let sequence = r.read_i64()?;
                let distance = r.read_f32()?;
                let flags = r.read_u8()?;
                let category = read_category(&mut r, flags)?;
                Packet::LocationSound(LocationSound {
                    channel_id,
                    sender,
                    position: [x, y, z],
                    data,
                    sequence,
                    distance,
                    category,
                })
            }
            ID_AUTHENTICATE => {
                let player = r.read_uuid()?;
                let secret = r.read_fixed_secret()?;
                Packet::Authenticate(Authenticate { player, secret })
            }
            ID_AUTHENTICATE_ACK => Packet::AuthenticateAck,
            ID_PING => {
                let id = r.read_uuid()?;
                let timestamp = r.read_i64()?;
                Packet::Ping(Ping { id, timestamp })
            }
            ID_KEEP_ALIVE => Packet::KeepAlive,
            ID_CONNECTION_CHECK => Packet::ConnectionCheck,
            ID_CONNECTION_CHECK_ACK => Packet::ConnectionCheckAck,
            other => return Err(PacketError::UnknownId(other)),
        };
        r.ensure_end()?;
        Ok(packet)
    }
}

fn read_category(r: &mut Reader<'_>, flags: u8) -> Result<Option<String>, PacketError> {
    if flags & HAS_CATEGORY_MASK != 0 {
        Ok(Some(r.read_string(MAX_SOUND_CATEGORY_CHARS)?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: i64) -> Uuid {
        Uuid::from_parts(n, n + 1)
    }

    fn roundtrip(p: Packet) -> Packet {
        let bytes = p.encode().unwrap();
        Packet::decode(&bytes).unwrap()
    }

    #[test]
    fn all_packet_ids_are_stable() {
        let cases: [(Packet, u8); 10] = [
            (
                Packet::Mic(Mic {
                    data: vec![],
                    sequence: 0,
                    whispering: false,
                }),
                0x01,
            ),
            (
                Packet::PlayerSound(PlayerSound {
                    channel_id: uuid(1),
                    sender: uuid(2),
                    data: vec![],
                    sequence: 0,
                    distance: 48.0,
                    whispering: false,
                    category: None,
                }),
                0x02,
            ),
            (
                Packet::GroupSound(GroupSound {
                    channel_id: uuid(1),
                    sender: uuid(2),
                    data: vec![],
                    sequence: 0,
                    category: None,
                }),
                0x03,
            ),
            (
                Packet::LocationSound(LocationSound {
                    channel_id: uuid(1),
                    sender: uuid(2),
                    position: [0.0; 3],
                    data: vec![],
                    sequence: 0,
                    distance: 48.0,
                    category: None,
                }),
                0x04,
            ),
            (
                Packet::Authenticate(Authenticate {
                    player: uuid(1),
                    secret: [0u8; 16],
                }),
                0x05,
            ),
            (Packet::AuthenticateAck, 0x06),
            (
                Packet::Ping(Ping {
                    id: uuid(3),
                    timestamp: 0,
                }),
                0x07,
            ),
            (Packet::KeepAlive, 0x08),
            (Packet::ConnectionCheck, 0x09),
            (Packet::ConnectionCheckAck, 0x0A),
        ];
        for (packet, id) in cases {
            assert_eq!(packet.id(), id);
            assert_eq!(packet.encode().unwrap()[0], id);
        }
    }

    #[test]
    fn mic_known_bytes() {
        let p = Packet::Mic(Mic {
            data: vec![0xAA, 0xBB],
            sequence: 1,
            whispering: false,
        });
        assert_eq!(
            p.encode().unwrap(),
            vec![
                0x01, // id
                0x02, 0xAA, 0xBB, // varint len + opus frame
                0, 0, 0, 0, 0, 0, 0, 1, // sequence = 1
                0x00, // whispering = false
            ]
        );
    }

    #[test]
    fn mic_empty_frame_is_stop_marker() {
        let p = Packet::Mic(Mic::stop(7));
        assert!(p.encode().unwrap() == {
            let mut v = vec![0x01, 0x00];
            v.extend_from_slice(&7i64.to_be_bytes());
            v.push(0);
            v
        });
        match roundtrip(p) {
            Packet::Mic(m) => {
                assert!(m.is_stop());
                assert_eq!(m.sequence, 7);
            }
            other => panic!("wrong packet {other:?}"),
        }
    }

    #[test]
    fn mic_roundtrip_with_max_payload_and_negative_sequence() {
        let p = Packet::Mic(Mic {
            data: vec![0x5A; MAX_OPUS_PAYLOAD],
            sequence: -3,
            whispering: true,
        });
        assert_eq!(roundtrip(p.clone()), p);
    }

    #[test]
    fn mic_payload_over_limit_is_refused_on_encode() {
        let p = Packet::Mic(Mic {
            data: vec![0; MAX_OPUS_PAYLOAD + 1],
            sequence: 0,
            whispering: false,
        });
        assert_eq!(
            p.encode(),
            Err(PacketError::Wire(WireError::ByteArrayLimit {
                len: MAX_OPUS_PAYLOAD + 1,
                max: MAX_OPUS_PAYLOAD
            }))
        );
    }

    #[test]
    fn mic_payload_over_limit_is_refused_on_decode() {
        let mut bytes = vec![0x01, 0x80, 0x0A]; // varint 1280
        bytes.extend(std::iter::repeat(0u8).take(MAX_OPUS_PAYLOAD + 5));
        assert!(matches!(
            Packet::decode(&bytes),
            Err(PacketError::Wire(WireError::ByteArrayLimit { .. }))
        ));
    }

    #[test]
    fn player_sound_known_bytes_without_category() {
        let p = Packet::PlayerSound(PlayerSound {
            channel_id: Uuid::NIL,
            sender: Uuid::NIL,
            data: vec![0x42],
            sequence: 5,
            distance: 48.0,
            whispering: false,
            category: None,
        });
        let mut expected = vec![0x02];
        expected.extend_from_slice(&[0u8; 32]); // two UUIDs
        expected.extend_from_slice(&[0x01, 0x42]); // data
        expected.extend_from_slice(&5i64.to_be_bytes());
        expected.extend_from_slice(&48.0f32.to_be_bytes()); // 0x42 0x40 0x00 0x00
        expected.push(0x00); // flags: no whisper, no category
        assert_eq!(p.encode().unwrap(), expected);
        // id(1) + uuids(32) + data(2) + seq(8) = 43 -> distance occupies 43..47
        assert_eq!(&expected[43..47], &[0x42, 0x40, 0x00, 0x00]);
        assert_eq!(expected.len(), 48);
    }

    #[test]
    fn player_sound_roundtrip_all_flag_combinations() {
        for whispering in [false, true] {
            for category in [None, Some("ui".to_string()), Some("weather".to_string())] {
                let p = Packet::PlayerSound(PlayerSound {
                    channel_id: uuid(10),
                    sender: uuid(20),
                    data: vec![1, 2, 3],
                    sequence: 99,
                    distance: 24.0,
                    whispering,
                    category: category.clone(),
                });
                assert_eq!(roundtrip(p.clone()), p);
            }
        }
    }

    #[test]
    fn player_sound_flags_byte_layout() {
        let base = PlayerSound {
            channel_id: uuid(1),
            sender: uuid(2),
            data: vec![],
            sequence: 0,
            distance: 48.0,
            whispering: false,
            category: None,
        };
        let mut loud = base.clone();
        loud.whispering = true;
        let mut cat = base.clone();
        cat.category = Some("x".into());
        let mut both = loud.clone();
        both.category = Some("x".into());

        let flags_at = |p: &PlayerSound| {
            let b = Packet::PlayerSound(p.clone()).encode().unwrap();
            // id(1) + 2 UUIDs(32) + len(1) + seq(8) + d(4) = 46
            b[46]
        };
        assert_eq!(flags_at(&base), 0b00);
        assert_eq!(flags_at(&loud), WHISPER_MASK);
        assert_eq!(flags_at(&cat), HAS_CATEGORY_MASK);
        assert_eq!(flags_at(&both), WHISPER_MASK | HAS_CATEGORY_MASK);
    }

    #[test]
    fn group_sound_roundtrip_and_flag_byte() {
        let p = Packet::GroupSound(GroupSound {
            channel_id: uuid(3),
            sender: uuid(4),
            data: vec![9; 16],
            sequence: 1234,
            category: Some("music".into()),
        });
        let bytes = p.encode().unwrap();
        assert_eq!(bytes[0], 0x03);
        // flags sits after id, 2 UUIDs, data(1+16) and seq(8)
        assert_eq!(bytes[1 + 32 + 17 + 8], HAS_CATEGORY_MASK);
        assert_eq!(roundtrip(p.clone()), p);

        let plain = Packet::GroupSound(GroupSound {
            channel_id: uuid(3),
            sender: uuid(4),
            data: vec![],
            sequence: 0,
            category: None,
        });
        assert_eq!(plain.encode().unwrap().last(), Some(&0x00));
        assert_eq!(roundtrip(plain.clone()), plain);
    }

    #[test]
    fn location_sound_field_order_is_coords_before_data() {
        let p = Packet::LocationSound(LocationSound {
            channel_id: Uuid::NIL,
            sender: Uuid::NIL,
            position: [1.0, 64.0, -1.0],
            data: vec![0x77],
            sequence: 2,
            distance: 48.0,
            category: None,
        });
        let bytes = p.encode().unwrap();
        let mut expected = vec![0x04];
        expected.extend_from_slice(&[0u8; 32]);
        expected.extend_from_slice(&1.0f64.to_be_bytes());
        expected.extend_from_slice(&64.0f64.to_be_bytes());
        expected.extend_from_slice(&(-1.0f64).to_be_bytes());
        expected.extend_from_slice(&[0x01, 0x77]);
        expected.extend_from_slice(&2i64.to_be_bytes());
        expected.extend_from_slice(&48.0f32.to_be_bytes());
        expected.push(0);
        assert_eq!(bytes, expected);
        assert_eq!(roundtrip(p.clone()), p);
    }

    #[test]
    fn location_sound_roundtrip_with_category() {
        let p = Packet::LocationSound(LocationSound {
            channel_id: uuid(5),
            sender: uuid(6),
            position: [-12.5, 70.25, 1e6],
            data: vec![0xAB; 60],
            sequence: -1,
            distance: 16.0,
            category: Some("ambient".into()),
        });
        assert_eq!(roundtrip(p.clone()), p);
    }

    #[test]
    fn authenticate_known_bytes() {
        let player = Uuid::from_parts(0x0102_0304_0506_0708, 0x1112_1314_1516_1718);
        let p = Packet::Authenticate(Authenticate {
            player,
            secret: [0xEE; 16],
        });
        let bytes = p.encode().unwrap();
        assert_eq!(bytes.len(), 1 + 16 + 16);
        assert_eq!(bytes[0], 0x05);
        assert_eq!(&bytes[1..17], &player.to_bytes());
        assert_eq!(&bytes[17..33], &[0xEE; 16]);
        assert_eq!(roundtrip(p.clone()), p);
    }

    #[test]
    fn authenticate_truncated_secret_is_an_error() {
        let mut bytes = vec![0x05];
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(&[0u8; 15]); // one byte short
        assert!(matches!(
            Packet::decode(&bytes),
            Err(PacketError::Wire(WireError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn ping_carries_uuid_and_epoch_millis() {
        let id = Uuid::parse_str("58bc9ae9-c7a8-45e4-a11c-efbb67199425").unwrap();
        let p = Packet::Ping(Ping {
            id,
            timestamp: 1_700_000_000_000,
        });
        let bytes = p.encode().unwrap();
        assert_eq!(bytes.len(), 1 + 16 + 8);
        assert_eq!(bytes[0], 0x07);
        assert_eq!(&bytes[1..17], &id.to_bytes());
        // The echo must be byte-identical (§5.7).
        assert_eq!(roundtrip(p.clone()), p);
        assert_eq!(Packet::Ping(roundtrip(p).into_ping().unwrap()).encode().unwrap(), bytes);
    }

    impl Packet {
        fn into_ping(self) -> Option<Ping> {
            match self {
                Packet::Ping(p) => Some(p),
                _ => None,
            }
        }
    }

    #[test]
    fn empty_payload_packets_are_one_byte() {
        for (p, id) in [
            (Packet::AuthenticateAck, 0x06u8),
            (Packet::KeepAlive, 0x08),
            (Packet::ConnectionCheck, 0x09),
            (Packet::ConnectionCheckAck, 0x0A),
        ] {
            assert_eq!(p.encode().unwrap(), vec![id]);
            assert_eq!(roundtrip(p.clone()), p);
        }
    }

    #[test]
    fn unknown_id_is_rejected() {
        assert_eq!(Packet::decode(&[0x00]), Err(PacketError::UnknownId(0)));
        assert_eq!(Packet::decode(&[0x0B]), Err(PacketError::UnknownId(0x0B)));
        assert_eq!(Packet::decode(&[0xFF]), Err(PacketError::UnknownId(0xFF)));
    }

    #[test]
    fn empty_input_is_an_error_not_a_panic() {
        assert!(matches!(
            Packet::decode(&[]),
            Err(PacketError::Wire(WireError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn trailing_bytes_after_a_complete_packet_are_rejected() {
        let mut bytes = Packet::KeepAlive.encode().unwrap();
        bytes.push(0x00);
        assert_eq!(
            Packet::decode(&bytes),
            Err(PacketError::TrailingBytes { count: 1 })
        );
    }

    #[test]
    fn truncated_packets_never_panic() {
        let full = Packet::PlayerSound(PlayerSound {
            channel_id: uuid(1),
            sender: uuid(2),
            data: vec![7; 10],
            sequence: 42,
            distance: 48.0,
            whispering: true,
            category: Some("cat".into()),
        })
        .encode()
        .unwrap();
        for cut in 0..full.len() {
            let res = Packet::decode(&full[..cut]);
            assert!(res.is_err(), "prefix of length {cut} unexpectedly decoded");
        }
        assert!(Packet::decode(&full).is_ok());
    }

    #[test]
    fn category_is_absent_when_flag_bit_is_clear() {
        // Hand-built player sound with flags = 0 but *with* a trailing string;
        // a decoder that ignores the flag would consume it and succeed.
        let mut bytes = vec![0x02];
        bytes.extend_from_slice(&[0u8; 32]);
        bytes.push(0x00); // empty data
        bytes.extend_from_slice(&0i64.to_be_bytes());
        bytes.extend_from_slice(&48.0f32.to_be_bytes());
        bytes.push(0x00); // flags
        bytes.extend_from_slice(&[0x03, b'f', b'o', b'o']);
        assert_eq!(
            Packet::decode(&bytes),
            Err(PacketError::TrailingBytes { count: 4 })
        );
    }

    #[test]
    fn ttl_matches_spec() {
        assert_eq!(
            Packet::Mic(Mic::stop(0)).ttl_ms(),
            500,
            "MicPacket TTL is 500 ms (§5)"
        );
        assert_eq!(Packet::PlayerSound(PlayerSound {
            channel_id: Uuid::NIL,
            sender: Uuid::NIL,
            data: vec![],
            sequence: 0,
            distance: 48.0,
            whispering: false,
            category: None,
        })
        .ttl_ms(), 10_000);
        assert_eq!(Packet::KeepAlive.ttl_ms(), 10_000);
    }

    #[test]
    fn flags_unknown_bits_are_tolerated() {
        // Java only tests the masks, so unknown bits must not break parsing.
        let mut bytes = vec![0x03]; // group sound
        bytes.extend_from_slice(&[0u8; 32]);
        bytes.push(0x00);
        bytes.extend_from_slice(&0i64.to_be_bytes());
        bytes.push(0b1000_0000);
        let p = Packet::decode(&bytes).unwrap();
        match p {
            Packet::GroupSound(g) => assert_eq!(g.category, None),
            other => panic!("wrong packet {other:?}"),
        }
    }
}
