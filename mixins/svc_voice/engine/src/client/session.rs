//! The live client session: socket + the pure state machine from
//! `crate::protocol::session`, plus the mic outbox and the incoming-audio inbox.
//!
//! Everything time-related happens here and is measured off `Instant`, so the
//! state machine itself stays testable without a socket (§6).
//!
//! ```text
//! connect() ───▶ RequestedSecret
//! on_secret() ─▶ Authenticating ──0x06──▶ (0x09 every 1000 ms) ──0x0A──▶ Validated
//!                                                                         |
//!                                       mic outbox at 20 ms, keep-alive echo |
//!                                                                         v
//!                                                       10 × keepAlive silence ⇒ TimedOut
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crate::protocol::crypto::Secret;
use crate::protocol::handshake::SecretPayload;
use crate::protocol::packets::{
    Authenticate, GroupSound, LocationSound, Mic, Packet, Ping, PlayerSound, MAX_OPUS_PAYLOAD,
};
use crate::protocol::session::{
    Action, Event, Session, SessionConfig, State, MIC_FRAME_INTERVAL, MIC_TTL,
};
use crate::protocol::wire::Uuid;

use crate::client::transport::{TransportError, TransportStats, UdpTransport, DEFAULT_READ_TIMEOUT};

/// One decoded audio frame from the server, ready for the Opus decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundFrame {
    /// Audio channel; per-channel jitter-buffer state is keyed on this (§6.5).
    pub channel_id: Uuid,
    /// Speaker. For proximity audio this is the entity to look up (§7.4).
    pub sender: Uuid,
    /// Opus payload. Empty = "speaker stopped": reset the decoder, play nothing (§6.6).
    pub data: Vec<u8>,
    /// Speaker's sequence; `< 0` means plugin-injected raw PCM (§5.2).
    pub sequence: i64,
    /// Audible radius in blocks, authoritative for attenuation (§7.4).
    pub distance: f32,
    /// `Some` for `0x04` — explicit world coordinates (§5.4).
    pub position: Option<[f64; 3]>,
    /// `0x03`: group audio, played without positional attenuation (§5.3).
    pub group: bool,
    pub whispering: bool,
    pub category: Option<String>,
}

impl SoundFrame {
    /// A zero-length payload is the end-of-transmission marker (§5.1, §6.6).
    pub fn is_stop(&self) -> bool {
        self.data.is_empty()
    }
}

/// Inbound events, delivered on a bounded `mpsc` channel so an audio thread can
/// consume them without holding the session.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// `0x06`: the server accepted the secret and bound our source address (§5.6).
    Authenticated,
    /// `0x0A`: validated — audio flows from now on (§5.6).
    Validated,
    /// A server keep-alive arrived and was echoed (§5.8).
    KeepAliveEchoed,
    /// A `0x07` we sent came back; RTT measured locally (§5.7).
    PingEcho { id: Uuid, rtt: Duration },
    /// A server-initiated `0x07`, already echoed back byte-for-byte (§5.7).
    ServerPing { id: Uuid, timestamp: i64 },
    /// Relay audio.
    Sound(Box<SoundFrame>),
    /// `10 × keepAlive` without a server packet: the secret is dead (§6.2).
    TimedOut,
    /// A datagram that had to be dropped, with the reason. Never fatal.
    Dropped(DropReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// Not `0xFF`, bad length prefix, truncated, trailing bytes (§3).
    Frame(String),
    /// GCM tag did not verify: wrong secret or tampering (§4.3).
    Decrypt(String),
    /// Decrypted, but the packet id or body is unknown/invalid (§5).
    Packet(String),
    /// A packet type that only a client may send (`0x01`/`0x05`/`0x09`).
    NotServerBound(&'static str),
    /// Nothing was reading the event channel; the frame was discarded rather
    /// than growing memory without bound.
    InboxFull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    Transport(String),
    Resolve(String),
    /// Mic payload longer than `min(mtu, 1275)` (§3.3, §7.1).
    FrameTooLarge {
        len: usize,
        max: usize,
    },
    /// The mic outbox is full: the caller produces frames faster than 20 ms.
    OutboxFull {
        capacity: usize,
    },
    /// Framing/encryption of an outgoing packet failed (should not happen).
    Encode(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Transport(e) => write!(f, "transport: {e}"),
            ClientError::Resolve(e) => write!(f, "resolve: {e}"),
            ClientError::FrameTooLarge { len, max } => {
                write!(f, "mic frame of {len} bytes exceeds the {max}-byte cap")
            }
            ClientError::OutboxFull { capacity } => {
                write!(f, "mic outbox full ({capacity} frames)")
            }
            ClientError::Encode(e) => write!(f, "encode: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<TransportError> for ClientError {
    fn from(e: TransportError) -> Self {
        match e {
            TransportError::Io(e) => ClientError::Transport(e.to_string()),
            other => ClientError::Transport(other.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Timings of the pure state machine (auth retry, timeout multiplier, ...).
    pub session: SessionConfig,
    /// Socket read timeout; bounds how long one [`ClientSession::pump`] blocks.
    pub read_timeout: Duration,
    /// Maximum number of mic frames buffered by the outbox.
    pub mic_queue_capacity: usize,
    /// Maximum number of undelivered [`Incoming`] events.
    pub inbox_capacity: usize,
    /// Upper bound of datagrams consumed per pump, so a flood cannot starve
    /// the timers.
    pub max_datagrams_per_pump: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            session: SessionConfig::default(),
            read_timeout: DEFAULT_READ_TIMEOUT,
            mic_queue_capacity: 128,
            inbox_capacity: 1024,
            max_datagrams_per_pump: 128,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub auth_sent: u64,
    pub connection_check_sent: u64,
    pub keep_alives_sent: u64,
    pub pings_sent: u64,
    pub ping_echoes: u64,
    pub server_pings: u64,
    pub mic_frames_sent: u64,
    /// Frames dropped because they waited longer than the 500 ms mic TTL (§5).
    pub mic_stale: u64,
    /// Frames refused by [`ClientSession::push_mic`].
    pub mic_refused: u64,
    pub mic_bytes_sent: u64,
    pub sound_frames: u64,
    /// `0x03` frames, the group channel: the server only sends these to
    /// members, so this is what proves a join took effect.
    pub group_sound_frames: u64,
    pub sound_bytes: u64,
    pub stop_frames: u64,
    pub dropped: u64,
    pub timeouts: u64,
    pub events_dropped: u64,
    pub transport: TransportStats,
}

struct QueuedMic {
    data: Vec<u8>,
    whispering: bool,
    queued_at: Instant,
}

pub struct ClientSession {
    cfg: ClientConfig,
    transport: UdpTransport,
    machine: Session,
    person: Uuid,
    secret: Secret,
    /// `min(mtu, 1275)` — the largest Opus payload we may emit (§3.3, §7.1).
    mic_cap: usize,
    mtu: i32,
    outbox: VecDeque<QueuedMic>,
    next_mic_due: Option<Instant>,
    sequence: i64,
    pending_pings: Vec<(Uuid, Instant)>,
    inbox_tx: SyncSender<Incoming>,
    inbox_rx: Option<Receiver<Incoming>>,
    stats: Stats,
    needs_secret: bool,
}

impl fmt::Debug for ClientSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientSession")
            .field("state", &self.machine.state())
            .field("player", &self.person)
            .field("endpoint", &self.transport.endpoint())
            .field("mtu", &self.mtu)
            .field("stats", &self.stats)
            .finish()
    }
}

impl ClientSession {
    /// Bind a socket to `endpoint` and arm the session from a
    /// `voicechat:secret` payload. The first `0x05` is sent here, because the
    /// state machine emits `SendAuthenticate` on `SecretReceived` (§6.1).
    pub fn connect<A: ToSocketAddrs>(
        endpoint: A,
        payload: &SecretPayload,
        cfg: ClientConfig,
    ) -> Result<Self, ClientError> {
        let addr: SocketAddr = endpoint
            .to_socket_addrs()
            .map_err(|e| ClientError::Resolve(e.to_string()))?
            .next()
            .ok_or_else(|| ClientError::Resolve("no address".into()))?;
        let mut transport = UdpTransport::connect(addr)?;
        transport
            .set_read_timeout(cfg.read_timeout)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        Self::with_transport(transport, payload, cfg)
    }

    /// Same, with a caller-supplied socket (pre-bound source port, tests).
    pub fn with_transport(
        transport: UdpTransport,
        payload: &SecretPayload,
        cfg: ClientConfig,
    ) -> Result<Self, ClientError> {
        let (tx, rx) = mpsc::sync_channel(cfg.inbox_capacity.max(1));
        let mut s = ClientSession {
            machine: Session::new(cfg.session.clone()),
            cfg,
            transport,
            person: payload.player_uuid,
            secret: Secret::from_bytes(payload.secret),
            mic_cap: mic_cap(payload.mtu_size),
            mtu: payload.mtu_size,
            outbox: VecDeque::new(),
            next_mic_due: None,
            sequence: 0,
            pending_pings: Vec::new(),
            inbox_tx: tx,
            inbox_rx: Some(rx),
            stats: Stats::default(),
            needs_secret: false,
        };
        s.begin(payload);
        Ok(s)
    }

    /// The receiving half of the event channel. Call once; later calls return
    /// `None`. Without a receiver the channel still accepts up to
    /// `inbox_capacity` events and then counts the drops.
    pub fn take_inbox(&mut self) -> Option<Receiver<Incoming>> {
        self.inbox_rx.take()
    }

    fn begin(&mut self, payload: &SecretPayload) {
        let now = Instant::now();
        self.machine.on_event(now, Event::RequestSecret);
        let actions = self.machine.on_secret(now, payload);
        self.dispatch(now, actions);
    }

    // ---- introspection -----------------------------------------------------

    pub fn state(&self) -> State {
        self.machine.state()
    }

    /// `Validated`: `0x0A` seen, audio may be sent and received (§5.6).
    pub fn is_connected(&self) -> bool {
        self.machine.is_connected()
    }

    /// `0x06` seen, `0x09` being retried (§6.1).
    pub fn is_authenticated(&self) -> bool {
        self.machine.is_authenticated()
    }

    pub fn keep_alive_interval(&self) -> Duration {
        self.machine.keep_alive_interval()
    }

    /// Death threshold, `10 × keepAlive` (§6.2).
    pub fn timeout(&self) -> Duration {
        self.machine.timeout()
    }

    pub fn mtu(&self) -> i32 {
        self.mtu
    }

    pub fn mic_cap(&self) -> usize {
        self.mic_cap
    }

    pub fn player_uuid(&self) -> Uuid {
        self.person
    }

    /// Length of the session key. The key itself is never exposed, so no log or
    /// error path can leak it (§4.1).
    pub fn secret_len(&self) -> usize {
        self.secret.as_bytes().len()
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.transport.endpoint()
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.transport.local_addr()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn queued_mic(&self) -> usize {
        self.outbox.len()
    }

    pub fn next_sequence(&self) -> i64 {
        self.sequence
    }

    /// True once a reconnect is required: the server dropped the secret and the
    /// caller has to ask for a new one over the Minecraft channel (§6.2, §2.5).
    pub fn needs_secret(&self) -> bool {
        self.needs_secret
    }

    // ---- driving -----------------------------------------------------------

    /// One loop iteration: drain the socket, advance the timers, send the mic
    /// frames that are due. Blocks at most `read_timeout` on an idle socket.
    ///
    /// Socket errors and malformed datagrams never abort the loop: this is a
    /// lossy channel, so they are counted and the session continues (§4.3,
    /// §6.3).
    pub fn pump(&mut self) -> Result<(), ClientError> {
        let now = Instant::now();
        let mut buf = [0u8; crate::client::transport::MAX_DATAGRAM_LEN];
        for _ in 0..self.cfg.max_datagrams_per_pump {
            match self.transport.recv(&mut buf) {
                Ok(Some(n)) => {
                    let datagram: Vec<u8> = buf[..n].to_vec();
                    self.handle_datagram(now, &datagram);
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        let actions = self.machine.on_event(now, Event::Tick);
        self.dispatch(now, actions);
        self.flush_mic(now);
        self.stats.transport = self.transport.stats();
        Ok(())
    }

    /// Pump until `duration` has elapsed. Convenience for a voice thread that
    /// has nothing else to do; Horizon will call [`Self::pump`] from its own
    /// tick loop instead.
    pub fn pump_for(&mut self, duration: Duration) -> Result<(), ClientError> {
        let start = Instant::now();
        while start.elapsed() < duration {
            self.pump()?;
        }
        Ok(())
    }

    /// Queue one Opus frame for transmission at the next 20 ms slot (§6.5).
    ///
    /// An empty `data` is legal and means "end of transmission" (§5.1).
    pub fn push_mic(&mut self, data: &[u8], whispering: bool) -> Result<(), ClientError> {
        if data.len() > self.mic_cap {
            self.stats.mic_refused += 1;
            return Err(ClientError::FrameTooLarge {
                len: data.len(),
                max: self.mic_cap,
            });
        }
        if self.outbox.len() >= self.cfg.mic_queue_capacity {
            self.stats.mic_refused += 1;
            return Err(ClientError::OutboxFull {
                capacity: self.cfg.mic_queue_capacity,
            });
        }
        self.outbox.push_back(QueuedMic {
            data: data.to_vec(),
            whispering,
            queued_at: Instant::now(),
        });
        Ok(())
    }

    /// Queue the mandatory zero-length "stopped talking" frame (§5.1).
    pub fn push_stop(&mut self) -> Result<(), ClientError> {
        self.push_mic(&[], false)
    }

    /// Send a `0x07` RTT probe. The echo produces [`Incoming::PingEcho`].
    ///
    /// The server only ever *answers* pings of its own `PingManager`; a
    /// client-sent ping is treated as a pong reply (§5.7, §6.3), so a missing
    /// echo is server policy, not a protocol error.
    pub fn send_ping(&mut self) -> Result<Uuid, ClientError> {
        let id = random_uuid()?;
        let timestamp = epoch_millis();
        self.pending_pings.push((id, Instant::now()));
        if self.pending_pings.len() > 16 {
            self.pending_pings.remove(0);
        }
        self.send_packet(&Packet::Ping(Ping { id, timestamp }))?;
        self.stats.pings_sent += 1;
        Ok(id)
    }

    /// Apply a new `voicechat:secret`: the old UDP session is discarded, the
    /// socket is kept (same source address), the sequence counter restarts at 0
    /// (§2.5, §6.5).
    pub fn on_secret(&mut self, payload: &SecretPayload) -> Result<(), ClientError> {
        let now = Instant::now();
        self.person = payload.player_uuid;
        self.secret = Secret::from_bytes(payload.secret);
        self.mtu = payload.mtu_size;
        self.mic_cap = mic_cap(payload.mtu_size);
        self.outbox.clear();
        self.next_mic_due = None;
        self.sequence = 0;
        self.pending_pings.clear();
        self.needs_secret = false;
        let actions = self.machine.on_secret(now, payload);
        self.dispatch(now, actions);
        Ok(())
    }

    /// Leave the session: nothing is sent to the server, the socket is just
    /// dropped (§6.4).
    pub fn disconnect(&mut self) {
        let now = Instant::now();
        let actions = self.machine.on_event(now, Event::Disconnected);
        self.dispatch(now, actions);
        self.outbox.clear();
        self.next_mic_due = None;
    }

    // ---- internals ---------------------------------------------------------

    fn send_packet(&mut self, packet: &Packet) -> Result<(), ClientError> {
        let datagram = crate::protocol::encode_outgoing(self.person, &self.secret, packet)
            .map_err(|e| ClientError::Encode(e.to_string()))?;
        self.transport.send(&datagram)?;
        Ok(())
    }

    fn dispatch(&mut self, _now: Instant, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::SendAuthenticate => {
                    if self
                        .send_packet(&Packet::Authenticate(Authenticate {
                            player: self.person,
                            secret: *self.secret.as_bytes(),
                        }))
                        .is_ok()
                    {
                        self.stats.auth_sent += 1;
                    }
                }
                Action::SendConnectionCheck => {
                    if self.send_packet(&Packet::ConnectionCheck).is_ok() {
                        self.stats.connection_check_sent += 1;
                    }
                }
                Action::SendKeepAlive => {
                    if self.send_packet(&Packet::KeepAlive).is_ok() {
                        self.stats.keep_alives_sent += 1;
                    }
                }
                Action::StartAudio => {
                    // §6.5: a fresh validation starts a fresh sequence space.
                    self.sequence = 0;
                    self.next_mic_due = None;
                }
                Action::StopAudio => {
                    self.next_mic_due = None;
                    self.outbox.clear();
                }
                Action::ReissueSecret => {
                    self.stats.timeouts += 1;
                    self.needs_secret = true;
                    self.pending_pings.clear();
                    self.emit(Incoming::TimedOut);
                }
            }
        }
    }

    fn handle_datagram(&mut self, now: Instant, datagram: &[u8]) {
        use crate::protocol::Error as E;
        match crate::protocol::decode_incoming(&self.secret, datagram) {
            Ok(packet) => self.on_packet(now, packet),
            Err(E::Frame(e)) => self.drop_with(DropReason::Frame(e.to_string())),
            Err(E::Crypto(e)) => self.drop_with(DropReason::Decrypt(e.to_string())),
            Err(E::Packet(e)) => self.drop_with(DropReason::Packet(e.to_string())),
        }
    }

    fn on_packet(&mut self, now: Instant, packet: Packet) {
        match packet {
            Packet::AuthenticateAck => {
                let before = self.machine.is_authenticated();
                let actions = self.machine.on_event(now, Event::AuthenticateAck);
                let accepted = !before && self.machine.is_authenticated();
                self.dispatch(now, actions);
                if accepted {
                    self.emit(Incoming::Authenticated);
                }
            }
            Packet::ConnectionCheckAck => {
                let before = self.machine.is_connected();
                let actions = self.machine.on_event(now, Event::ConnectionCheckAck);
                let accepted = !before && self.machine.is_connected();
                self.dispatch(now, actions);
                if accepted {
                    self.emit(Incoming::Validated);
                }
            }
            Packet::KeepAlive => {
                let live = self.machine.is_connected();
                let actions = self.machine.on_event(now, Event::KeepAliveReceived);
                self.dispatch(now, actions);
                if live {
                    self.emit(Incoming::KeepAliveEchoed);
                }
            }
            Packet::Ping(p) => {
                if let Some(idx) = self.pending_pings.iter().position(|(id, _)| *id == p.id) {
                    let (_, sent_at) = self.pending_pings.remove(idx);
                    self.stats.ping_echoes += 1;
                    self.emit(Incoming::PingEcho {
                        id: p.id,
                        rtt: now.saturating_duration_since(sent_at),
                    });
                } else {
                    // §5.7: the echo must be the identical packet.
                    if self.send_packet(&Packet::Ping(p)).is_ok() {
                        self.stats.server_pings += 1;
                        self.emit(Incoming::ServerPing {
                            id: p.id,
                            timestamp: p.timestamp,
                        });
                    }
                }
            }
            Packet::PlayerSound(PlayerSound {
                channel_id,
                sender,
                data,
                sequence,
                distance,
                whispering,
                category,
            }) => {
                self.count_sound(&data);
                self.emit(Incoming::Sound(Box::new(SoundFrame {
                    channel_id,
                    sender,
                    data,
                    sequence,
                    distance,
                    position: None,
                    group: false,
                    whispering,
                    category,
                })));
            }
            Packet::GroupSound(GroupSound {
                channel_id,
                sender,
                data,
                sequence,
                category,
            }) => {
                self.stats.group_sound_frames += 1;
                self.count_sound(&data);
                self.emit(Incoming::Sound(Box::new(SoundFrame {
                    channel_id,
                    sender,
                    data,
                    sequence,
                    distance: 0.0,
                    position: None,
                    group: true,
                    whispering: false,
                    category,
                })));
            }
            Packet::LocationSound(LocationSound {
                channel_id,
                sender,
                position,
                data,
                sequence,
                distance,
                category,
            }) => {
                self.count_sound(&data);
                self.emit(Incoming::Sound(Box::new(SoundFrame {
                    channel_id,
                    sender,
                    data,
                    sequence,
                    distance,
                    position: Some(position),
                    group: false,
                    whispering: false,
                    category,
                })));
            }
            Packet::Mic(_) => self.drop_with(DropReason::NotServerBound("0x01 MicPacket")),
            Packet::Authenticate(_) => {
                self.drop_with(DropReason::NotServerBound("0x05 Authenticate"))
            }
            Packet::ConnectionCheck => {
                self.drop_with(DropReason::NotServerBound("0x09 ConnectionCheck"))
            }
        }
    }

    fn count_sound(&mut self, data: &[u8]) {
        self.stats.sound_frames += 1;
        self.stats.sound_bytes += data.len() as u64;
        if data.is_empty() {
            self.stats.stop_frames += 1;
        }
    }

    fn drop_with(&mut self, reason: DropReason) {
        self.stats.dropped += 1;
        self.emit(Incoming::Dropped(reason));
    }

    fn emit(&mut self, event: Incoming) {
        match self.inbox_tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.stats.events_dropped += 1,
            Err(TrySendError::Disconnected(_)) => self.stats.events_dropped += 1,
        }
    }

    /// Send the frames that are due, one per 20 ms slot, dropping anything that
    /// waited longer than the 500 ms mic TTL.
    fn flush_mic(&mut self, now: Instant) {
        if !self.machine.can_send_mic() {
            self.next_mic_due = None;
            return;
        }
        loop {
            while let Some(front) = self.outbox.front() {
                if now.saturating_duration_since(front.queued_at) > MIC_TTL {
                    self.outbox.pop_front();
                    self.stats.mic_stale += 1;
                    continue;
                }
                break;
            }
            let due = self.next_mic_due.unwrap_or(now);
            if now < due {
                break;
            }
            let Some(front) = self.outbox.pop_front() else {
                self.next_mic_due = None;
                break;
            };
            let mic = Packet::Mic(Mic {
                data: front.data.clone(),
                sequence: self.sequence,
                whispering: front.whispering,
            });
            self.sequence += 1;
            if self.send_packet(&mic).is_ok() {
                self.stats.mic_frames_sent += 1;
                self.stats.mic_bytes_sent += front.data.len() as u64;
            }
            // Fixed 20 ms cadence, but never build up a burst after a stall.
            let next = due + MIC_FRAME_INTERVAL;
            let behind = now.saturating_duration_since(next);
            self.next_mic_due = Some(if behind > Duration::from_millis(100) {
                now
            } else {
                next
            });
        }
    }
}

/// Largest Opus payload this session may emit: the server's MTU but never more
/// than the protocol's 1275-byte ceiling (§3.3, §7.1).
fn mic_cap(mtu: i32) -> usize {
    mtu.max(1).min(MAX_OPUS_PAYLOAD as i32) as usize
}

fn random_uuid() -> Result<Uuid, ClientError> {
    let rand = Secret::generate().map_err(|e| ClientError::Encode(e.to_string()))?;
    Ok(Uuid::from_bytes(*rand.as_bytes()))
}

fn epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
