//! Encrypted UDP transport (mosh network.cc port): timestamped packets,
//! RFC 6298-style RTT estimation from the timestamp echo, and server-side
//! roaming (the server re-targets replies at the source address of the
//! newest authenticated datagram).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use crate::remote::crypto::{Direction, Key, Session};
use crate::util::{now_ms, Error, Result};

pub const DEFAULT_PORT_RANGE: (u16, u16) = (60001, 60999);
const MIN_RTO: u64 = 50; // ms
const MAX_RTO: u64 = 1000; // ms
// Floor RTTVAR at this fraction of SRTT when deriving the RTO. On a very stable
// link the measured RTTVAR collapses toward zero, leaving an RTO only a few ms
// above SRTT — tighter than the peer's own ack-generation jitter, so acks
// routinely land just after the RTO fires and trigger spurious retransmits
// (observed: srtt=102ms, rttvar≈1.25ms => rto=107ms, ~2ms over peak RTT). A
// proportional floor keeps the margin sensible relative to the round-trip time
// (≈4·frac·SRTT, e.g. 20ms on a 100ms link) instead of a flat constant that is
// too loose on a LAN and too tight across a continent. Read-time only — the
// stored RTTVAR keeps tracking true variance, so a link that turns jittery
// widens the RTO immediately. MIN_RTO still guards the low-SRTT (LAN) end.
const RTTVAR_FLOOR_FRAC: f64 = 0.05;
const TS_NONE: u16 = 0xffff;
// mosh transportsender SEND_INTERVAL_MIN/MAX: pacing derived from SRTT.
// MIN is also the server's floor between fresh frames.
pub const SEND_INTERVAL_MIN: u64 = 20; // ms
const SEND_INTERVAL_MAX: u64 = 250; // ms

/// Address family selection (-4 / -6 flags; mosh --family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Family {
    /// Dual-stack server bind when possible; client prefers IPv4.
    #[default]
    Auto,
    Inet,
    Inet6,
}

impl Family {
    /// Parses a `-4`/`-6` flag; None for anything else.
    pub fn from_flag(flag: &str) -> Option<Family> {
        match flag {
            "-4" => Some(Family::Inet),
            "-6" => Some(Family::Inet6),
            _ => None,
        }
    }
}

/// Binds an IPv6 UDP wildcard socket; `v6only=false` requests a dual-stack
/// socket that also accepts IPv4 (as v4-mapped addresses).
fn bind_udp_v6(port: u16, v6only: bool) -> std::io::Result<UdpSocket> {
    // SAFETY: socket/setsockopt/bind/fcntl on a plain integer fd; the
    // sockaddr is zero-initialized then fully set, and its size is passed
    // alongside. The fd is owned by the returned UdpSocket (or closed on
    // every error path).
    unsafe {
        // Linux can request close-on-exec atomically via the SOCK_CLOEXEC type
        // flag; macOS/BSD have no such constant, so set FD_CLOEXEC with a
        // follow-up fcntl. See docs/decisions/0001-posh-term-libc-portability.md.
        #[cfg(target_os = "linux")]
        let fd = libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        #[cfg(not(target_os = "linux"))]
        let fd = libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
        }
        let on: libc::c_int = v6only as libc::c_int;
        libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        let mut addr: libc::sockaddr_in6 = std::mem::zeroed();
        addr.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        addr.sin6_port = port.to_be();
        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        ) < 0
        {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

fn bind_server_socket(port: u16, family: Family) -> std::io::Result<UdpSocket> {
    match family {
        Family::Inet => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)),
        Family::Inet6 => bind_udp_v6(port, true),
        // Prefer one dual-stack socket; fall back to plain IPv4 on hosts
        // without IPv6.
        Family::Auto => {
            bind_udp_v6(port, false).or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)))
        }
    }
}

/// 16-bit wallclock used in packet timestamps; 0xffff is reserved for "none".
pub fn timestamp16(now: u64) -> u16 {
    let ts = (now % 65536) as u16;
    if ts == TS_NONE {
        0
    } else {
        ts
    }
}

pub fn timestamp_diff(tsnew: u16, tsold: u16) -> u16 {
    tsnew.wrapping_sub(tsold)
}

/// Smoothed RTT estimator (RFC 6298 constants, as in mosh).
pub struct RttEstimator {
    srtt: f64,
    rttvar: f64,
    hit: bool,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    pub fn new() -> RttEstimator {
        RttEstimator {
            srtt: 1000.0,
            rttvar: 500.0,
            hit: false,
        }
    }

    pub fn sample(&mut self, r: f64) {
        if r >= 5000.0 {
            // Ignore wild values (e.g. the peer was suspended).
            return;
        }
        if !self.hit {
            self.srtt = r;
            self.rttvar = r / 2.0;
            self.hit = true;
        } else {
            const ALPHA: f64 = 1.0 / 8.0;
            const BETA: f64 = 1.0 / 4.0;
            self.rttvar = (1.0 - BETA) * self.rttvar + BETA * (self.srtt - r).abs();
            self.srtt = (1.0 - ALPHA) * self.srtt + ALPHA * r;
        }
    }

    pub fn rto(&self) -> u64 {
        let rttvar = self.rttvar.max(self.srtt * RTTVAR_FLOOR_FRAC);
        let rto = (self.srtt + 4.0 * rttvar).ceil() as u64;
        rto.clamp(MIN_RTO, MAX_RTO)
    }

    pub fn srtt(&self) -> f64 {
        self.srtt
    }
}

pub struct Connection {
    sock: UdpSocket,
    session: Session,
    is_server: bool,
    remote: Option<SocketAddr>,
    saved_timestamp: Option<(u16, u64)>, // (peer timestamp, received_at)
    rtt: RttEstimator,
    /// Next sequence number expected from the peer (mosh
    /// expected_receiver_seq): only datagrams at or above it may update the
    /// timestamp echo, RTT estimate, or roamed remote address.
    expected_receiver_seq: u64,
    /// Cumulative sealed-datagram bytes sent and received on the wire. Read by
    /// the remote stats collector for the bandwidth figures; never affects
    /// transport behavior.
    bytes_tx: u64,
    bytes_rx: u64,
}

impl Connection {
    /// Server side: binds the first free UDP port in the range. With
    /// `Family::Auto` this is a dual-stack IPv6 socket when the host
    /// supports it, otherwise an IPv4 wildcard.
    pub fn server(range: (u16, u16), key: &Key, family: Family) -> Result<(Connection, u16)> {
        let (low, high) = range;
        if low > high || low == 0 {
            return Err(Error::from("invalid port range"));
        }
        for port in low..=high {
            if let Ok(sock) = bind_server_socket(port, family) {
                sock.set_nonblocking(true)?;
                let conn = Connection {
                    sock,
                    session: Session::new(key, Direction::ToClient),
                    is_server: true,
                    remote: None,
                    saved_timestamp: None,
                    rtt: RttEstimator::new(),
                    expected_receiver_seq: 0,
                    bytes_tx: 0,
                    bytes_rx: 0,
                };
                return Ok((conn, port));
            }
        }
        Err(Error(format!(
            "could not bind any UDP port in {low}:{high}"
        )))
    }

    pub fn client(remote: SocketAddr, key: &Key) -> Result<Connection> {
        let sock = match remote {
            SocketAddr::V4(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?,
            SocketAddr::V6(_) => UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?,
        };
        sock.set_nonblocking(true)?;
        Ok(Connection {
            sock,
            session: Session::new(key, Direction::ToServer),
            is_server: false,
            remote: Some(remote),
            saved_timestamp: None,
            rtt: RttEstimator::new(),
            expected_receiver_seq: 0,
            bytes_tx: 0,
            bytes_rx: 0,
        })
    }

    /// Test-only: simulate a network roam by rebinding the local socket to a
    /// fresh ephemeral port while keeping the AEAD session (the sequence counter
    /// keeps advancing). The next datagram then leaves from a new source address
    /// but in-sequence, so the server adopts it — exactly the mosh roaming path,
    /// which on a real host is the kernel changing the source address out from
    /// under the same socket (e.g. wifi → cellular).
    #[cfg(test)]
    pub fn roam_rebind(&mut self) -> std::io::Result<()> {
        let remote = self.remote.expect("a client connection has a remote");
        let sock = match remote {
            SocketAddr::V4(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?,
            SocketAddr::V6(_) => UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?,
        };
        sock.set_nonblocking(true)?;
        self.sock = sock;
        Ok(())
    }

    pub fn raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn has_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// The peer socket address datagrams are currently sent to (the address the
    /// last authenticated datagram was learned from). Surfaced for the SIGUSR2
    /// state dump: a stale value here vs. where the peer is really sending from
    /// is the signature of a roam the server hasn't re-pinned to.
    pub fn remote(&self) -> Option<SocketAddr> {
        self.remote
    }

    pub fn rto(&self) -> u64 {
        self.rtt.rto()
    }

    /// Smoothed RTT in milliseconds (mosh SRTT). Surfaced for the stats log.
    pub fn srtt(&self) -> f64 {
        self.rtt.srtt()
    }

    /// Cumulative sealed-datagram bytes sent / received on the wire.
    pub fn bytes_tx(&self) -> u64 {
        self.bytes_tx
    }
    pub fn bytes_rx(&self) -> u64 {
        self.bytes_rx
    }

    /// mosh's send interval: half the smoothed RTT, clamped. Drives the
    /// prediction engine's SRTT trigger.
    pub fn send_interval(&self) -> u64 {
        ((self.rtt.srtt() / 2.0).ceil() as u64).clamp(SEND_INTERVAL_MIN, SEND_INTERVAL_MAX)
    }

    /// Seals and sends one payload. Send errors are swallowed: with roaming,
    /// transient unreachability is normal and retransmission recovers.
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        let Some(remote) = self.remote else {
            return Ok(());
        };
        let now = now_ms();
        // Echo the most recently received peer timestamp, advanced by how
        // long we held it, so the peer can measure RTT from our reply.
        let reply = match self.saved_timestamp.take() {
            Some((ts, at)) if now.saturating_sub(at) < 1000 => ts.wrapping_add((now - at) as u16),
            other => {
                self.saved_timestamp = other;
                TS_NONE
            }
        };
        let mut packet = Vec::with_capacity(4 + payload.len());
        packet.extend_from_slice(&timestamp16(now).to_be_bytes());
        packet.extend_from_slice(&reply.to_be_bytes());
        packet.extend_from_slice(payload);
        let dgram = self.session.seal(&packet)?;
        self.bytes_tx += dgram.len() as u64;
        let _ = self.sock.send_to(&dgram, remote);
        Ok(())
    }

    /// Receives one datagram. Ok(None) means an unauthentic/replayed packet
    /// was dropped; WouldBlock propagates when the socket runs dry.
    pub fn recv(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 2048];
        let (n, from) = self.sock.recv_from(&mut buf)?;
        // Count every datagram that reached the socket (including ones that
        // fail to authenticate) as received wire bytes.
        self.bytes_rx += n as u64;
        let Ok((seq, plaintext)) = self.session.open(&buf[..n]) else {
            return Ok(None);
        };
        if plaintext.len() < 4 {
            return Ok(None);
        }
        // Late in-window reorders still deliver their payload, but only the
        // newest datagram may update the timestamp echo, RTT estimate, or
        // (server) the roamed remote address — a stale packet from a
        // pre-roam or spoofed source must not re-target the stream
        // (mosh network.cc expected_receiver_seq guard).
        if seq >= self.expected_receiver_seq {
            self.expected_receiver_seq = seq + 1;
            let now = now_ms();
            let ts = u16::from_be_bytes([plaintext[0], plaintext[1]]);
            let reply = u16::from_be_bytes([plaintext[2], plaintext[3]]);
            if ts != TS_NONE {
                self.saved_timestamp = Some((ts, now));
            }
            if reply != TS_NONE {
                self.rtt
                    .sample(timestamp_diff(timestamp16(now), reply) as f64);
            }
            if self.is_server {
                // Roaming: adopt the source of the newest authentic packet.
                self.remote = Some(from);
            }
        }
        Ok(Some(plaintext[4..].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_sockets_are_close_on_exec() {
        // The load-bearing invariant behind the platform split in
        // bind_udp_v6 (docs/decisions/0001-posh-term-libc-portability.md):
        // the encrypted-transport fd must never leak across spawn_shell's
        // exec into the user's shell. Linux sets it atomically via
        // SOCK_CLOEXEC, other platforms via a follow-up fcntl, and std
        // covers the sockets it creates — this pins the OUTCOME for every
        // Connection constructor path, whichever idiom produced it.
        let assert_cloexec = |fd: RawFd, what: &str| {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD failed for {what}");
            assert!(
                flags & libc::FD_CLOEXEC != 0,
                "{what} socket missing FD_CLOEXEC"
            );
        };
        let key = Key::random();
        let (server, _) = Connection::server((62600, 62699), &key, Family::Auto).unwrap();
        assert_cloexec(server.raw_fd(), "auto/dual-stack server");
        let (server4, port4) = Connection::server((62600, 62699), &key, Family::Inet).unwrap();
        assert_cloexec(server4.raw_fd(), "ipv4 server");
        if let Ok((server6, _)) = Connection::server((62600, 62699), &key, Family::Inet6) {
            assert_cloexec(server6.raw_fd(), "ipv6 server");
        }
        let client =
            Connection::client(format!("127.0.0.1:{port4}").parse().unwrap(), &key).unwrap();
        assert_cloexec(client.raw_fd(), "client");
    }

    #[test]
    fn rtt_first_sample_initializes() {
        let mut est = RttEstimator::new();
        est.sample(100.0);
        assert_eq!(est.srtt, 100.0);
        assert_eq!(est.rto(), 300); // 100 + 4 * 50
    }

    #[test]
    fn rtt_smooths_subsequent_samples() {
        let mut est = RttEstimator::new();
        est.sample(100.0);
        est.sample(200.0);
        // RTTVAR = 0.75*50 + 0.25*100 = 62.5; SRTT = 0.875*100 + 0.125*200 = 112.5
        assert!((est.srtt - 112.5).abs() < 1e-9);
        assert_eq!(est.rto(), 363); // ceil(112.5 + 250)
    }

    #[test]
    fn rtt_rto_clamped() {
        let mut est = RttEstimator::new();
        for _ in 0..10 {
            est.sample(1.0);
        }
        assert_eq!(est.rto(), MIN_RTO);
        let mut est = RttEstimator::new();
        est.sample(4000.0);
        assert_eq!(est.rto(), MAX_RTO);
    }

    #[test]
    fn rtt_ignores_outliers() {
        let mut est = RttEstimator::new();
        est.sample(100.0);
        est.sample(10_000.0);
        assert_eq!(est.srtt, 100.0);
    }

    #[test]
    fn rto_keeps_proportional_margin_on_stable_high_rtt_link() {
        // A perfectly stable ~100ms link drives RTTVAR toward zero. Without the
        // floor the RTO would sit ~1ms above SRTT and spuriously retransmit on
        // the peer's ordinary ack jitter (the transatlantic-Tailscale case that
        // motivated the floor). The proportional RTTVAR floor keeps the margin
        // tied to the round-trip time: srtt + 4·(srtt·0.05) = srtt·1.2 ≈ 120ms.
        let mut est = RttEstimator::new();
        for _ in 0..200 {
            est.sample(100.0);
        }
        assert!((est.srtt() - 100.0).abs() < 1.0, "srtt should converge to ~100ms");
        let rto = est.rto();
        assert!(
            (118..=125).contains(&rto),
            "expected ~120ms RTO (≥2x the bare srtt+ε), got {rto}ms"
        );
    }

    #[test]
    fn timestamp_diff_wraps() {
        assert_eq!(timestamp_diff(5, 65530), 11);
        assert_eq!(timestamp_diff(100, 40), 60);
    }

    #[test]
    fn family_flag_parsing() {
        assert_eq!(Family::from_flag("-4"), Some(Family::Inet));
        assert_eq!(Family::from_flag("-6"), Some(Family::Inet6));
        assert_eq!(Family::from_flag("-5"), None);
        assert_eq!(Family::from_flag("--ipv4"), None);
    }

    #[test]
    fn send_interval_tracks_srtt() {
        let key = Key::random();
        let (conn, _) = Connection::server((61400, 61499), &key, Family::Inet).unwrap();
        // Initial SRTT is 1000ms -> clamped to the 250ms max.
        assert_eq!(conn.send_interval(), 250);
        let mut est = RttEstimator::new();
        est.sample(10.0);
        assert_eq!(est.srtt(), 10.0);
        let interval = ((est.srtt() / 2.0).ceil() as u64).clamp(20, 250);
        assert_eq!(interval, 20); // clamped to the minimum
    }

    #[test]
    fn ipv6_loopback_roundtrip() {
        let key = Key::random();
        // Skip quietly on hosts without IPv6.
        let Ok((mut server, port)) = Connection::server((61700, 61799), &key, Family::Inet6) else {
            return;
        };
        let addr: SocketAddr = format!("[::1]:{port}").parse().unwrap();
        assert!(addr.is_ipv6());
        let mut client = Connection::client(addr, &key).unwrap();
        client.send(b"v6 hello").unwrap();
        assert_eq!(recv_one(&mut server), b"v6 hello");
    }

    #[test]
    fn dual_stack_accepts_ipv4_client() {
        let key = Key::random();
        let (mut server, port) = Connection::server((61800, 61899), &key, Family::Auto).unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut client = Connection::client(addr, &key).unwrap();
        client.send(b"v4 over auto").unwrap();
        assert_eq!(recv_one(&mut server), b"v4 over auto");
        assert!(server.has_remote());
    }

    /// Drains one payload out of a nonblocking connection.
    fn recv_one(conn: &mut Connection) -> Vec<u8> {
        for _ in 0..50 {
            match conn.recv() {
                Ok(Some(p)) => return p,
                Ok(None) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("recv: {e}"),
            }
        }
        panic!("datagram never arrived");
    }

    #[test]
    fn stale_datagram_does_not_re_target_roaming() {
        let key = Key::random();
        let (mut server, port) = Connection::server((62000, 62099), &key, Family::Inet).unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Seal two client->server packets from one session, then deliver
        // them out of order from two different source addresses.
        let mut session = Session::new(&key, Direction::ToServer);
        let seal = |payload: &[u8], s: &mut Session| {
            let mut pt = Vec::new();
            pt.extend_from_slice(&TS_NONE.to_be_bytes());
            pt.extend_from_slice(&TS_NONE.to_be_bytes());
            pt.extend_from_slice(payload);
            s.seal(&pt).unwrap()
        };
        let first = seal(b"first", &mut session); // seq 0
        let second = seal(b"second", &mut session); // seq 1

        let new_addr = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let old_addr = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();

        new_addr.send_to(&second, addr).unwrap();
        assert_eq!(recv_one(&mut server), b"second");
        assert_eq!(server.remote, Some(new_addr.local_addr().unwrap()));

        // The late seq-0 packet (in-window reorder) from a different source
        // still delivers its payload, but must not re-point the server at
        // the stale address.
        old_addr.send_to(&first, addr).unwrap();
        assert_eq!(recv_one(&mut server), b"first");
        assert_eq!(
            server.remote,
            Some(new_addr.local_addr().unwrap()),
            "stale datagram re-targeted the roaming address"
        );
    }

    #[test]
    fn loopback_roundtrip_with_roaming_adoption() {
        let key = Key::random();
        let (mut server, port) = Connection::server((61500, 61999), &key, Family::Inet).unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut client = Connection::client(addr, &key).unwrap();

        client.send(b"hello").unwrap();
        assert_eq!(recv_one(&mut server), b"hello");
        assert!(server.has_remote());

        server.send(b"world").unwrap();
        assert_eq!(recv_one(&mut client), b"world");

        // Wire-byte counters reflect the sealed datagrams that crossed the
        // socket (payload + 4-byte timestamp header + AEAD overhead), so each
        // side's tx and the peer's rx are both non-zero after a roundtrip.
        assert!(client.bytes_tx() > b"hello".len() as u64);
        assert!(server.bytes_rx() >= client.bytes_tx());
        assert!(server.bytes_tx() > b"world".len() as u64);
        assert!(client.bytes_rx() >= server.bytes_tx());
    }
}
