//! Pure session state machine (SPEC §6), no I/O and no clock.
//!
//! The caller owns the socket and the wall clock: every call passes `now`.
//! The machine returns the [`Action`]s the caller has to perform, in order.
//!
//! Lifecycle (§1.1, §6.1):
//!
//! ```text
//! Disconnected --RequestSecret--> RequestedSecret
//! RequestedSecret --secret--> Authenticating --0x06--> Authenticating
//! Authenticating --0x0A--> Validated --timeout--> TimedOut
//! ```
//!
//! The two-phase auth is the part reimplementations usually get wrong: after
//! `0x06` the client must keep sending `0x09` every 1000 ms until `0x0A`,
//! otherwise it stays silently "half connected" (§6.1).

use std::time::{Duration, Instant};

use crate::protocol::handshake::SecretPayload;

/// Authentication / connection-check retry period (§6.1, §6.2).
pub const AUTH_RETRY: Duration = Duration::from_millis(1000);
/// Timeout multiplier applied to the keep-alive interval (§6.2).
pub const TIMEOUT_MULTIPLIER: u32 = 10;
/// Default keep-alive period if the secret packet does not say otherwise (§6.2).
pub const DEFAULT_KEEP_ALIVE: Duration = Duration::from_millis(1000);
/// Microphone frame TTL: a frame queued for longer than this is useless (§5).
pub const MIC_TTL: Duration = Duration::from_millis(500);
/// One Opus frame covers 20 ms (§7.1).
pub const MIC_FRAME_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No session, no secret.
    Disconnected,
    /// `voicechat:request_secret` was sent, waiting for `voicechat:secret`.
    RequestedSecret,
    /// Secret known; sending `0x05` and then `0x09` every 1000 ms.
    Authenticating,
    /// `0x0A` received: audio may flow, keep-alives are exchanged.
    Validated,
    /// `10 × keepAlive` elapsed without a server packet: the secret is dead.
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendAuthenticate,
    SendConnectionCheck,
    SendKeepAlive,
    StartAudio,
    StopAudio,
    /// The session died; the caller must fetch a new secret (§6.4).
    ReissueSecret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The caller sent `voicechat:request_secret` (world join).
    RequestSecret,
    /// A `voicechat:secret` payload arrived; carries the keep-alive period.
    SecretReceived { keep_alive: Duration },
    /// `0x06` — secret accepted, address bound.
    AuthenticateAck,
    /// `0x0A` — connection validated.
    ConnectionCheckAck,
    /// `0x08` from the server.
    KeepAliveReceived,
    /// Periodic poll; the only event that can advance timers.
    Tick,
    /// Left the world / socket closed.
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub auth_retry: Duration,
    pub timeout_multiplier: u32,
    pub keep_alive: Duration,
    /// Optional "no SVC here" detection (§2.4). `None` (default) never gives up,
    /// which is what the reference client does.
    pub secret_timeout: Option<Duration>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            auth_retry: AUTH_RETRY,
            timeout_multiplier: TIMEOUT_MULTIPLIER,
            keep_alive: DEFAULT_KEEP_ALIVE,
            secret_timeout: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    cfg: SessionConfig,
    state: State,
    keep_alive: Duration,
    timeout: Duration,
    /// When the next `0x05`/`0x09` is due, if any.
    next_retry: Option<Instant>,
    /// Last server packet that proves the connection is alive (`0x08`/`0x0A`).
    last_server: Option<Instant>,
    /// When the current state was entered.
    state_since: Option<Instant>,
    /// `0x06` seen, so `0x09` is being retried.
    authenticated: bool,
}

impl Default for Session {
    fn default() -> Self {
        Session::new(SessionConfig::default())
    }
}

impl Session {
    pub fn new(cfg: SessionConfig) -> Self {
        let keep_alive = cfg.keep_alive;
        let timeout = keep_alive * cfg.timeout_multiplier;
        Session {
            cfg,
            state: State::Disconnected,
            keep_alive,
            timeout,
            next_retry: None,
            last_server: None,
            state_since: None,
            authenticated: false,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Validated
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// The keep-alive period currently in force (from the secret).
    pub fn keep_alive_interval(&self) -> Duration {
        self.keep_alive
    }

    /// `10 × keepAlive` (§6.2).
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn mic_ttl(&self) -> Duration {
        MIC_TTL
    }

    pub fn mic_frame_interval(&self) -> Duration {
        MIC_FRAME_INTERVAL
    }

    /// MicPackets are only processed for validated connections (§6.3).
    pub fn can_send_mic(&self) -> bool {
        self.state == State::Validated
    }

    /// Convenience: feed a parsed `voicechat:secret` payload.
    pub fn on_secret(&mut self, now: Instant, payload: &SecretPayload) -> Vec<Action> {
        self.on_event(
            now,
            Event::SecretReceived {
                keep_alive: payload.effective_keep_alive(),
            },
        )
    }

    /// Advance the machine. `now` must be monotonic.
    ///
    /// `SPEC-AMBIGUITY`: §6.1 says the auth/check loop retries forever with no
    /// maximum count, so `Authenticating` has no timeout; only `Validated` uses
    /// the `10 × keepAlive` death threshold (§6.2).
    pub fn on_event(&mut self, now: Instant, event: Event) -> Vec<Action> {
        match event {
            Event::RequestSecret => {
                self.state = State::RequestedSecret;
                self.state_since = Some(now);
                self.next_retry = None;
                self.last_server = None;
                self.authenticated = false;
                Vec::new()
            }
            Event::SecretReceived { keep_alive } => {
                let restarting = matches!(self.state, State::Validated | State::Authenticating);
                self.keep_alive = if keep_alive.is_zero() {
                    Duration::from_millis(1)
                } else {
                    keep_alive
                };
                self.timeout = self.keep_alive * self.cfg.timeout_multiplier;
                self.state = State::Authenticating;
                self.state_since = Some(now);
                self.authenticated = false;
                self.last_server = None;
                self.next_retry = Some(now + self.cfg.auth_retry);
                let mut actions = Vec::new();
                if restarting {
                    // §2.5: a new secret discards the previous UDP session.
                    actions.push(Action::StopAudio);
                }
                actions.push(Action::SendAuthenticate);
                actions
            }
            Event::AuthenticateAck => {
                if self.state == State::Authenticating && !self.authenticated {
                    self.authenticated = true;
                    self.next_retry = Some(now + self.cfg.auth_retry);
                    vec![Action::SendConnectionCheck]
                } else {
                    Vec::new()
                }
            }
            Event::ConnectionCheckAck => {
                if self.state == State::Authenticating && self.authenticated {
                    self.state = State::Validated;
                    self.state_since = Some(now);
                    self.last_server = Some(now);
                    self.next_retry = None;
                    vec![Action::StartAudio]
                } else {
                    // §6.1: 0x0A is only accepted after 0x06.
                    Vec::new()
                }
            }
            Event::KeepAliveReceived => {
                if self.state == State::Validated {
                    self.last_server = Some(now);
                    vec![Action::SendKeepAlive]
                } else {
                    Vec::new()
                }
            }
            Event::Tick => self.on_tick(now),
            Event::Disconnected => {
                let was_active = matches!(
                    self.state,
                    State::Authenticating | State::Validated
                );
                self.state = State::Disconnected;
                self.state_since = Some(now);
                self.next_retry = None;
                self.last_server = None;
                self.authenticated = false;
                if was_active {
                    vec![Action::StopAudio]
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn on_tick(&mut self, now: Instant) -> Vec<Action> {
        match self.state {
            State::Authenticating => {
                let due = matches!(self.next_retry, Some(t) if now >= t);
                if !due {
                    return Vec::new();
                }
                self.next_retry = Some(now + self.cfg.auth_retry);
                if self.authenticated {
                    vec![Action::SendConnectionCheck]
                } else {
                    vec![Action::SendAuthenticate]
                }
            }
            State::Validated => {
                let last = self.last_server.unwrap_or(now);
                // §6.2 uses a strict ">" comparison against 10 × keepAlive.
                if now.saturating_duration_since(last) > self.timeout {
                    self.state = State::TimedOut;
                    self.state_since = Some(now);
                    self.next_retry = None;
                    vec![Action::StopAudio, Action::ReissueSecret]
                } else {
                    Vec::new()
                }
            }
            State::RequestedSecret => {
                if let (Some(limit), Some(since)) = (self.cfg.secret_timeout, self.state_since) {
                    if now.saturating_duration_since(since) > limit {
                        self.state = State::Disconnected;
                        self.state_since = Some(now);
                    }
                }
                Vec::new()
            }
            State::Disconnected | State::TimedOut => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::handshake::Codec;
    use crate::protocol::wire::Uuid;

    fn t0() -> Instant {
        Instant::now()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn payload(keep_alive: i32) -> SecretPayload {
        SecretPayload {
            secret: [0x11; 16],
            server_port: 24454,
            player_uuid: Uuid::from_parts(1, 2),
            codec: Codec::Voip,
            mtu_size: 1275,
            voice_chat_distance: 48.0,
            keep_alive,
            groups_enabled: false,
            voice_host: String::new(),
            allow_recording: false,
        }
    }

    /// Drive the machine to `Validated` and return the instant of validation.
    fn validated(s: &mut Session, base: Instant) -> Instant {
        s.on_event(base, Event::RequestSecret);
        s.on_secret(base, &payload(1000));
        s.on_event(base, Event::AuthenticateAck);
        s.on_event(base, Event::ConnectionCheckAck);
        assert_eq!(s.state(), State::Validated);
        base
    }

    #[test]
    fn starts_disconnected_and_idle() {
        let mut s = Session::default();
        assert_eq!(s.state(), State::Disconnected);
        assert!(!s.is_connected());
        assert!(!s.can_send_mic());
        assert!(s.on_event(t0(), Event::Tick).is_empty());
        assert_eq!(s.state(), State::Disconnected);
    }

    #[test]
    fn request_secret_moves_to_requested_state() {
        let mut s = Session::default();
        let base = t0();
        assert!(s.on_event(base, Event::RequestSecret).is_empty());
        assert_eq!(s.state(), State::RequestedSecret);
        // No secret ever arrives: the machine must not invent retries (§2.2).
        assert!(s.on_event(base + ms(60_000), Event::Tick).is_empty());
        assert_eq!(s.state(), State::RequestedSecret);
    }

    #[test]
    fn secret_starts_authenticating_and_sends_immediately() {
        let mut s = Session::default();
        let base = t0();
        s.on_event(base, Event::RequestSecret);
        let actions = s.on_secret(base, &payload(1000));
        assert_eq!(actions, vec![Action::SendAuthenticate]);
        assert_eq!(s.state(), State::Authenticating);
        assert!(!s.can_send_mic());
        assert!(!s.is_connected());
    }

    #[test]
    fn authenticate_retries_after_exactly_one_second() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        assert!(s.on_event(base + ms(999), Event::Tick).is_empty());
        assert_eq!(
            s.on_event(base + ms(1000), Event::Tick),
            vec![Action::SendAuthenticate]
        );
        // And again one period after the *send*, not after the start.
        assert!(s.on_event(base + ms(1500), Event::Tick).is_empty());
        assert_eq!(
            s.on_event(base + ms(2000), Event::Tick),
            vec![Action::SendAuthenticate]
        );
    }

    #[test]
    fn authenticate_ack_switches_to_connection_checks() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        assert_eq!(
            s.on_event(base + ms(10), Event::AuthenticateAck),
            vec![Action::SendConnectionCheck]
        );
        assert!(s.is_authenticated());
        assert_eq!(
            s.on_event(base + ms(1010), Event::Tick),
            vec![Action::SendConnectionCheck]
        );
    }

    #[test]
    fn a_repeated_authenticate_ack_does_not_double_send() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        s.on_event(base, Event::AuthenticateAck);
        assert!(s.on_event(base, Event::AuthenticateAck).is_empty());
    }

    #[test]
    fn connection_check_ack_validates_and_starts_audio() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        s.on_event(base, Event::AuthenticateAck);
        assert_eq!(
            s.on_event(base, Event::ConnectionCheckAck),
            vec![Action::StartAudio]
        );
        assert_eq!(s.state(), State::Validated);
        assert!(s.is_connected());
        assert!(s.can_send_mic());
    }

    #[test]
    fn connection_check_ack_before_authenticate_is_ignored() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        assert!(s.on_event(base, Event::ConnectionCheckAck).is_empty());
        assert_eq!(s.state(), State::Authenticating);
        assert!(s.on_event(base, Event::Disconnected).contains(&Action::StopAudio));
    }

    #[test]
    fn keep_alive_is_echoed_and_refreshes_the_timer() {
        let mut s = Session::default();
        let base = t0();
        validated(&mut s, base);
        assert_eq!(
            s.on_event(base + ms(1000), Event::KeepAliveReceived),
            vec![Action::SendKeepAlive]
        );
        // 9.5 s after the keep-alive: still alive.
        assert!(s.on_event(base + ms(10_500), Event::Tick).is_empty());
        assert_eq!(s.state(), State::Validated);
    }

    #[test]
    fn keep_alive_before_validation_is_not_answered() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        assert!(s.on_event(base, Event::KeepAliveReceived).is_empty());
    }

    #[test]
    fn timeout_fires_one_millisecond_after_ten_keep_alives() {
        let mut s = Session::default();
        let base = t0();
        validated(&mut s, base);
        assert_eq!(s.timeout(), ms(10_000));
        assert!(s.on_event(base + ms(10_000), Event::Tick).is_empty());
        assert_eq!(
            s.on_event(base + ms(10_001), Event::Tick),
            vec![Action::StopAudio, Action::ReissueSecret]
        );
        assert_eq!(s.state(), State::TimedOut);
        assert!(!s.can_send_mic());
        // And it stays dead until a new secret arrives.
        assert!(s.on_event(base + ms(20_000), Event::Tick).is_empty());
    }

    #[test]
    fn timeout_scales_with_the_keep_alive_from_the_secret() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(2500));
        s.on_event(base, Event::AuthenticateAck);
        s.on_event(base, Event::ConnectionCheckAck);
        assert_eq!(s.timeout(), ms(25_000));
        assert!(s.on_event(base + ms(20_000), Event::Tick).is_empty());
        assert_eq!(s.state(), State::Validated);
        assert_eq!(
            s.on_event(base + ms(25_001), Event::Tick),
            vec![Action::StopAudio, Action::ReissueSecret]
        );
    }

    #[test]
    fn keep_alive_period_is_clamped_to_the_server_minimum() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(200));
        assert_eq!(s.keep_alive_interval(), ms(1000));
        // A hostile/zero value must not produce a zero-length timeout.
        let mut s2 = Session::default();
        s2.on_secret(base, &payload(0));
        assert!(s2.keep_alive_interval() >= ms(1));
    }

    #[test]
    fn tick_does_not_spam_retries() {
        let mut s = Session::default();
        let base = t0();
        s.on_secret(base, &payload(1000));
        let mut sends = 1; // the initial SendAuthenticate
        for step in 1..=5000u64 {
            if s.on_event(base + ms(step), Event::Tick) == vec![Action::SendAuthenticate] {
                sends += 1;
            }
        }
        // 5 s of ticking at 1 kHz with a 1 s retry -> the first one plus five.
        assert_eq!(sends, 6, "expected 1 Hz retry, got {sends} sends");
    }

    #[test]
    fn disconnect_stops_audio_and_clears_everything() {
        let mut s = Session::default();
        let base = t0();
        validated(&mut s, base);
        assert_eq!(
            s.on_event(base + ms(100), Event::Disconnected),
            vec![Action::StopAudio]
        );
        assert_eq!(s.state(), State::Disconnected);
        assert!(!s.can_send_mic());
        assert!(s.on_event(base + ms(200), Event::Tick).is_empty());
    }

    #[test]
    fn disconnect_from_idle_state_emits_nothing() {
        let mut s = Session::default();
        assert!(s.on_event(t0(), Event::Disconnected).is_empty());
    }

    #[test]
    fn a_late_secret_restarts_the_udp_session() {
        let mut s = Session::default();
        let base = t0();
        validated(&mut s, base);
        let actions = s.on_secret(base + ms(3000), &payload(1000));
        assert_eq!(actions, vec![Action::StopAudio, Action::SendAuthenticate]);
        assert_eq!(s.state(), State::Authenticating);
        assert!(!s.can_send_mic());
    }

    #[test]
    fn secret_while_requesting_does_not_emit_stop_audio() {
        let mut s = Session::default();
        let base = t0();
        s.on_event(base, Event::RequestSecret);
        assert_eq!(
            s.on_secret(base + ms(50), &payload(1000)),
            vec![Action::SendAuthenticate]
        );
    }

    #[test]
    fn mic_ttl_and_frame_interval_match_the_spec() {
        let s = Session::default();
        assert_eq!(s.mic_ttl(), ms(500));
        assert_eq!(s.mic_frame_interval(), ms(20));
    }

    #[test]
    fn optional_secret_timeout_detects_a_server_without_svc() {
        let mut s = Session::new(SessionConfig {
            secret_timeout: Some(ms(3000)),
            ..SessionConfig::default()
        });
        let base = t0();
        s.on_event(base, Event::RequestSecret);
        assert!(s.on_event(base + ms(3000), Event::Tick).is_empty());
        assert_eq!(s.state(), State::RequestedSecret);
        s.on_event(base + ms(3001), Event::Tick);
        assert_eq!(s.state(), State::Disconnected);
    }

    #[test]
    fn a_full_happy_path_with_a_steady_keep_alive_stream() {
        let mut s = Session::default();
        let base = t0();
        validated(&mut s, base);
        for round in 1..=60u64 {
            let now = base + ms(round * 1000);
            assert_eq!(
                s.on_event(now, Event::KeepAliveReceived),
                vec![Action::SendKeepAlive]
            );
            assert!(s.on_event(now + ms(10), Event::Tick).is_empty());
        }
        assert_eq!(s.state(), State::Validated);
    }
}
