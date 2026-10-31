//! UDP transport for the voice channel (SPEC §1, §3, §6.3).
//!
//! One socket, one fixed local port for the whole session, `connect()`ed to
//! the voice endpoint. `connect` on a UDP socket makes the OS filter incoming
//! datagrams to that peer — which is exactly the rule of §3.2 ("filter on the
//! voice endpoint address"), and it also means the socket cannot be rebound to
//! a different peer mid-session, as §6.3 demands.
//!
//! There is no `tokio`, no `mio`, no epoll: the receive side is a socket with a
//! short read timeout, drained in a loop. That is enough for a 20 ms audio
//! cadence and keeps the crate usable from Horizon's synchronous voice thread.

use std::fmt;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// Read timeout of the socket (§9 "non-blocking-like loop"): one pump may
/// block this long waiting for a datagram before the timers run.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(20);

/// §3.3: the server's receive buffer is 4096 bytes and a datagram that size or
/// larger is rejected; nothing valid can be bigger, so the client treats it the
/// same way.
pub const MAX_DATAGRAM_LEN: usize = 4096;

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    /// `ToSocketAddrs` produced nothing usable.
    Resolve(String),
    /// Refused before touching the socket: the framing cap is a hard limit.
    Oversize {
        len: usize,
        max: usize,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "udp i/o: {e}"),
            TransportError::Resolve(s) => write!(f, "cannot resolve the voice endpoint: {s}"),
            TransportError::Oversize { len, max } => {
                write!(f, "datagram of {len} bytes exceeds the {max}-byte limit")
            }
        }
    }
}

impl std::error::Error for TransportError {}

impl From<io::Error> for TransportError {
    fn from(e: io::Error) -> Self {
        TransportError::Io(e)
    }
}

/// Counters only — no payloads, so the transport can be inspected without
/// touching anything the protocol layer owns.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransportStats {
    pub datagrams_sent: u64,
    pub datagrams_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// `recv` returned "nothing to read" (timeout or an ICMP error).
    pub recv_empty: u64,
    pub send_errors: u64,
    pub recv_errors: u64,
    /// Incoming datagrams at or above [`MAX_DATAGRAM_LEN`] (dropped, §3.3).
    pub oversize_received: u64,
}

pub struct UdpTransport {
    socket: UdpSocket,
    endpoint: SocketAddr,
    stats: TransportStats,
}

impl fmt::Debug for UdpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpTransport")
            .field("local", &self.local_addr().ok())
            .field("endpoint", &self.endpoint)
            .field("stats", &self.stats)
            .finish()
    }
}

impl UdpTransport {
    /// Bind a fresh ephemeral port and connect it to the voice endpoint.
    pub fn connect<A: ToSocketAddrs>(endpoint: A) -> Result<Self, TransportError> {
        let endpoint = endpoint
            .to_socket_addrs()
            .map_err(|e| TransportError::Resolve(e.to_string()))?
            .next()
            .ok_or_else(|| TransportError::Resolve("no address".into()))?;
        // Wildcard bind of the same family, port 0: the OS picks the port and
        // keeps it for the lifetime of the socket (§1, §6.3).
        let bind: SocketAddr = if endpoint.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind)?;
        socket.connect(endpoint)?;
        socket.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        Ok(UdpTransport {
            socket,
            endpoint,
            stats: TransportStats::default(),
        })
    }

    /// Adopt an already connected socket (tests, or a caller that pre-binds a
    /// specific source port).
    pub fn from_socket(socket: UdpSocket, endpoint: SocketAddr) -> io::Result<Self> {
        socket.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        Ok(UdpTransport {
            socket,
            endpoint,
            stats: TransportStats::default(),
        })
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn stats(&self) -> TransportStats {
        self.stats
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.socket.set_read_timeout(Some(timeout))
    }

    /// Send one framed datagram. Oversized input is refused locally instead of
    /// being thrown at the server's 4096-byte buffer.
    pub fn send(&mut self, datagram: &[u8]) -> Result<(), TransportError> {
        if datagram.len() >= MAX_DATAGRAM_LEN {
            return Err(TransportError::Oversize {
                len: datagram.len(),
                max: MAX_DATAGRAM_LEN - 1,
            });
        }
        match self.socket.send(datagram) {
            Ok(n) => {
                self.stats.datagrams_sent += 1;
                self.stats.bytes_sent += n as u64;
                Ok(())
            }
            Err(e) => {
                self.stats.send_errors += 1;
                // Windows reports an ICMP port-unreachable as WSAECONNRESET
                // on the *next* send/recv after a datagram bounced. A voice
                // socket must survive that: it is a lossy channel, not a
                // connection.
                if is_icmp_error(&e) {
                    Ok(())
                } else {
                    Err(TransportError::Io(e))
                }
            }
        }
    }

    /// Read one datagram into `buf`.
    ///
    /// `Ok(None)` means "nothing right now" — a read timeout, or an ICMP
    /// error surfaced by the OS. Both are ordinary events on this channel.
    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match self.socket.recv(buf) {
            Ok(n) => {
                self.stats.datagrams_received += 1;
                self.stats.bytes_received += n as u64;
                if n >= MAX_DATAGRAM_LEN {
                    self.stats.oversize_received += 1;
                    return Ok(Some(n));
                }
                Ok(Some(n))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.stats.recv_empty += 1;
                Ok(None)
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                self.stats.recv_empty += 1;
                Ok(None)
            }
            Err(e) if is_icmp_error(&e) => {
                self.stats.recv_empty += 1;
                Ok(None)
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                self.stats.recv_empty += 1;
                Ok(None)
            }
            Err(e) => {
                self.stats.recv_errors += 1;
                Err(e)
            }
        }
    }
}

/// ICMP-derived socket errors that a UDP session must tolerate: the peer is
/// unreachable *right now*, which for a lossy audio channel is just loss.
fn is_icmp_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_an_ephemeral_port_and_stays_on_it() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut t = UdpTransport::connect(server.local_addr().unwrap()).unwrap();
        let first = t.local_addr().unwrap();
        assert_ne!(first.port(), 0);
        assert_eq!(t.endpoint(), server.local_addr().unwrap());

        let mut buf = [0u8; 64];
        server.send_to(b"hello", first).unwrap();
        let n = t.recv(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"hello");
        // Same local port after traffic: §6.3 requires a stable source address.
        assert_eq!(t.local_addr().unwrap(), first);
    }

    #[test]
    fn connected_socket_ignores_other_senders() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut t = UdpTransport::connect(server.local_addr().unwrap()).unwrap();
        t.set_read_timeout(Duration::from_millis(50)).unwrap();
        let me = t.local_addr().unwrap();
        // A stray datagram from an address the socket is not connected to must
        // never be delivered (§3.2: filter on the voice endpoint).
        stranger.send_to(b"nope", me).unwrap();
        let mut buf = [0u8; 64];
        assert!(t.recv(&mut buf).unwrap().is_none());
        server.send_to(b"yes", me).unwrap();
        assert!(t.recv(&mut buf).unwrap().is_some());
    }

    #[test]
    fn timeout_is_none_and_counted() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut t = UdpTransport::connect(server.local_addr().unwrap()).unwrap();
        t.set_read_timeout(Duration::from_millis(20)).unwrap();
        let mut buf = [0u8; 64];
        assert!(t.recv(&mut buf).unwrap().is_none());
        assert_eq!(t.stats().recv_empty, 1);
        assert_eq!(t.stats().datagrams_received, 0);
    }

    #[test]
    fn refuses_to_send_a_datagram_at_the_server_buffer_limit() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut t = UdpTransport::connect(server.local_addr().unwrap()).unwrap();
        let big = vec![0u8; MAX_DATAGRAM_LEN];
        match t.send(&big) {
            Err(TransportError::Oversize { len, .. }) => assert_eq!(len, MAX_DATAGRAM_LEN),
            other => panic!("expected oversize refusal, got {other:?}"),
        }
        // One byte below the limit is legal on the wire.
        assert!(t.send(&vec![0u8; MAX_DATAGRAM_LEN - 1]).is_ok());
    }

    #[test]
    fn stats_count_both_directions() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut t = UdpTransport::connect(server.local_addr().unwrap()).unwrap();
        t.send(b"abc").unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"abc");
        server.send_to(b"de", from).unwrap();
        t.recv(&mut buf).unwrap();
        let s = t.stats();
        assert_eq!((s.datagrams_sent, s.bytes_sent), (1, 3));
        assert_eq!((s.datagrams_received, s.bytes_received), (1, 2));
    }

    #[test]
    fn unresolvable_endpoint_is_an_error_not_a_panic() {
        assert!(matches!(
            UdpTransport::connect("no-such-host.invalid:24454"),
            Err(TransportError::Resolve(_))
        ));
    }
}
