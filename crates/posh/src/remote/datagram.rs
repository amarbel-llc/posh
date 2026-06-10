//! Encrypted UDP transport (mosh network.cc port): timestamped packets,
//! RFC 6298-style RTT estimation from the timestamp echo, and server-side
//! roaming (the server re-targets replies at the source address of the last
//! authenticated datagram).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};

use crate::remote::crypto::{Direction, Key, Session};
use crate::util::{now_ms, Error, Result};

pub const DEFAULT_PORT_RANGE: (u16, u16) = (60001, 60999);
const MIN_RTO: u64 = 50; // ms
const MAX_RTO: u64 = 1000; // ms
const TS_NONE: u16 = 0xffff;

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
        let rto = (self.srtt + 4.0 * self.rttvar).ceil() as u64;
        rto.clamp(MIN_RTO, MAX_RTO)
    }
}

pub struct Connection {
    sock: UdpSocket,
    session: Session,
    is_server: bool,
    remote: Option<SocketAddr>,
    saved_timestamp: Option<(u16, u64)>, // (peer timestamp, received_at)
    rtt: RttEstimator,
}

impl Connection {
    /// Server side: binds the first free UDP port in the range (IPv4
    /// wildcard; mosh additionally tries per-family binds).
    pub fn server(range: (u16, u16), key: &Key) -> Result<(Connection, u16)> {
        let (low, high) = range;
        if low > high || low == 0 {
            return Err(Error::from("invalid port range"));
        }
        for port in low..=high {
            if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
                sock.set_nonblocking(true)?;
                let conn = Connection {
                    sock,
                    session: Session::new(key, Direction::ToClient),
                    is_server: true,
                    remote: None,
                    saved_timestamp: None,
                    rtt: RttEstimator::new(),
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
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn has_remote(&self) -> bool {
        self.remote.is_some()
    }

    pub fn rto(&self) -> u64 {
        self.rtt.rto()
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
        let _ = self.sock.send_to(&dgram, remote);
        Ok(())
    }

    /// Receives one datagram. Ok(None) means an unauthentic/replayed packet
    /// was dropped; WouldBlock propagates when the socket runs dry.
    pub fn recv(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 2048];
        let (n, from) = self.sock.recv_from(&mut buf)?;
        let Ok((_seq, plaintext)) = self.session.open(&buf[..n]) else {
            return Ok(None);
        };
        if plaintext.len() < 4 {
            return Ok(None);
        }
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
            // Roaming: adopt the source address of the last authentic packet.
            self.remote = Some(from);
        }
        Ok(Some(plaintext[4..].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn timestamp_diff_wraps() {
        assert_eq!(timestamp_diff(5, 65530), 11);
        assert_eq!(timestamp_diff(100, 40), 60);
    }

    #[test]
    fn loopback_roundtrip_with_roaming_adoption() {
        let key = Key::random();
        let (mut server, port) = Connection::server((61500, 61999), &key).unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut client = Connection::client(addr, &key).unwrap();

        client.send(b"hello").unwrap();
        // Loopback delivery is immediate but give it a moment.
        let mut got = None;
        for _ in 0..50 {
            match server.recv() {
                Ok(Some(p)) => {
                    got = Some(p);
                    break;
                }
                Ok(None) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("recv: {e}"),
            }
        }
        assert_eq!(got.as_deref(), Some(&b"hello"[..]));
        assert!(server.has_remote());

        server.send(b"world").unwrap();
        let mut got = None;
        for _ in 0..50 {
            match client.recv() {
                Ok(Some(p)) => {
                    got = Some(p);
                    break;
                }
                Ok(None) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("recv: {e}"),
            }
        }
        assert_eq!(got.as_deref(), Some(&b"world"[..]));
    }
}
