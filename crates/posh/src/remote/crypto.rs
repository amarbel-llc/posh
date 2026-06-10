//! Datagram encryption (mosh crypto.cc port, with AES-128-GCM substituted
//! for AES-OCB).
//!
//! Wire format of a sealed datagram:
//!   [8 bytes big-endian nonce value][AES-GCM ciphertext + 16-byte tag]
//! The nonce value packs the direction in the top bit and a 63-bit sequence
//! number below it (mosh's Nonce); the AEAD nonce is that value zero-padded
//! to 96 bits. Keys are 128-bit, printed as 22 chars of unpadded base64.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use posh_term::base64;
use rand::rngs::OsRng;
use rand::RngCore;

use crate::util::{Error, Result};

pub const KEY_LEN: usize = 16;
const TAG_LEN: usize = 16;
const NONCE_PREFIX_LEN: usize = 8;
pub const SEAL_OVERHEAD: usize = NONCE_PREFIX_LEN + TAG_LEN;

const DIRECTION_MASK: u64 = 1 << 63;
const SEQUENCE_MASK: u64 = !DIRECTION_MASK;

#[derive(Clone)]
pub struct Key(pub [u8; KEY_LEN]);

impl Key {
    pub fn random() -> Key {
        let mut k = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut k);
        Key(k)
    }

    /// mosh-style printable key: exactly 22 base64 chars, no padding.
    pub fn to_base64(&self) -> String {
        let mut s = base64::encode(&self.0);
        s.truncate(s.trim_end_matches('=').len());
        s
    }

    pub fn from_base64(s: &str) -> Result<Key> {
        if s.len() != 22 {
            return Err(Error::from("key must be 22 letters long"));
        }
        let bytes = base64::decode(s.as_bytes())
            .ok_or_else(|| Error::from("key must be well-formed base64"))?;
        let arr: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|_| Error::from("key must decode to 16 bytes"))?;
        Ok(Key(arr))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToServer,
    ToClient,
}

impl Direction {
    fn bit(self) -> u64 {
        match self {
            Direction::ToClient => DIRECTION_MASK,
            Direction::ToServer => 0,
        }
    }
}

fn nonce_bytes(val: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&val.to_be_bytes());
    n
}

/// Anti-replay window: rejects sequence numbers at or below the highest seen,
/// except inside a 64-entry reorder window tracked by bitmap.
struct ReplayWindow {
    highest: u64,
    bitmap: u64,
    initialized: bool,
}

impl ReplayWindow {
    fn new() -> ReplayWindow {
        ReplayWindow {
            highest: 0,
            bitmap: 0,
            initialized: false,
        }
    }

    fn check(&mut self, seq: u64) -> Result<()> {
        if !self.initialized {
            self.highest = seq;
            self.bitmap = 1;
            self.initialized = true;
            return Ok(());
        }
        if seq > self.highest {
            let shift = seq - self.highest;
            self.bitmap = if shift >= 64 { 0 } else { self.bitmap << shift };
            self.bitmap |= 1;
            self.highest = seq;
            return Ok(());
        }
        let diff = self.highest - seq;
        if diff >= 64 {
            return Err(Error::from("sequence number too old"));
        }
        if self.bitmap & (1 << diff) != 0 {
            return Err(Error::from("replayed sequence number"));
        }
        self.bitmap |= 1 << diff;
        Ok(())
    }
}

pub struct Session {
    cipher: Aes128Gcm,
    send_dir: Direction,
    next_seq: u64,
    replay: ReplayWindow,
}

impl Session {
    pub fn new(key: &Key, send_dir: Direction) -> Session {
        Session {
            cipher: Aes128Gcm::new_from_slice(&key.0).expect("16-byte key"),
            send_dir,
            next_seq: 0,
            replay: ReplayWindow::new(),
        }
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let val = self.send_dir.bit() | (self.next_seq & SEQUENCE_MASK);
        self.next_seq += 1;
        let nonce = nonce_bytes(val);
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| Error::from("encryption failed"))?;
        let mut out = Vec::with_capacity(NONCE_PREFIX_LEN + ct.len());
        out.extend_from_slice(&val.to_be_bytes());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Authenticates and decrypts a datagram from the peer. Rejects packets
    /// sealed in our own direction (reflection) and replays.
    pub fn open(&mut self, datagram: &[u8]) -> Result<(u64, Vec<u8>)> {
        if datagram.len() < SEAL_OVERHEAD {
            return Err(Error::from("datagram too short"));
        }
        let val = u64::from_be_bytes(datagram[..NONCE_PREFIX_LEN].try_into().unwrap());
        let dir = if val & DIRECTION_MASK != 0 {
            Direction::ToClient
        } else {
            Direction::ToServer
        };
        if dir == self.send_dir {
            return Err(Error::from("wrong packet direction"));
        }
        let nonce = nonce_bytes(val);
        let pt = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce), &datagram[NONCE_PREFIX_LEN..])
            .map_err(|_| Error::from("authentication failed"))?;
        let seq = val & SEQUENCE_MASK;
        // Replay accounting happens only after successful authentication so
        // forged packets cannot poison the window.
        self.replay.check(seq)?;
        Ok((seq, pt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Session, Session) {
        let key = Key::random();
        (
            Session::new(&key, Direction::ToClient),
            Session::new(&key, Direction::ToServer),
        )
    }

    #[test]
    fn key_base64_roundtrip() {
        let key = Key::random();
        let s = key.to_base64();
        assert_eq!(s.len(), 22);
        assert!(!s.contains('='));
        assert_eq!(Key::from_base64(&s).unwrap().0, key.0);
        assert!(Key::from_base64("short").is_err());
        assert!(Key::from_base64("!!!!!!!!!!!!!!!!!!!!!!").is_err());
    }

    #[test]
    fn seal_open_roundtrip() {
        let (mut server, mut client) = pair();
        let dgram = server.seal(b"hello roaming world").unwrap();
        let (seq, pt) = client.open(&dgram).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(pt, b"hello roaming world");
        // And the other direction.
        let dgram = client.seal(b"keystrokes").unwrap();
        assert_eq!(server.open(&dgram).unwrap().1, b"keystrokes");
    }

    #[test]
    fn tamper_rejected() {
        let (mut server, mut client) = pair();
        let mut dgram = server.seal(b"payload").unwrap();
        let last = dgram.len() - 1;
        dgram[last] ^= 0x01;
        assert!(client.open(&dgram).is_err());
        // Flipping a nonce byte must also fail authentication.
        let mut dgram = server.seal(b"payload").unwrap();
        dgram[7] ^= 0x01;
        assert!(client.open(&dgram).is_err());
    }

    #[test]
    fn replay_rejected() {
        let (mut server, mut client) = pair();
        let dgram = server.seal(b"once").unwrap();
        assert!(client.open(&dgram).is_ok());
        assert!(client.open(&dgram).is_err());
    }

    #[test]
    fn reorder_window_allows_recent_then_rejects_old() {
        let (mut server, mut client) = pair();
        let d0 = server.seal(b"0").unwrap();
        let d1 = server.seal(b"1").unwrap();
        let d2 = server.seal(b"2").unwrap();
        // Out-of-order delivery within the window is fine.
        assert!(client.open(&d2).is_ok());
        assert!(client.open(&d0).is_ok());
        assert!(client.open(&d1).is_ok());
        // ...but each only once.
        assert!(client.open(&d1).is_err());
        // Anything older than the 64-packet window is rejected.
        let mut old = None;
        for i in 0..70 {
            let d = server.seal(b"x").unwrap();
            if i == 3 {
                old = Some(d);
            } else {
                let _ = client.open(&d);
            }
        }
        assert!(client.open(&old.unwrap()).is_err());
    }

    #[test]
    fn reflection_rejected() {
        let key = Key::random();
        let mut server = Session::new(&key, Direction::ToClient);
        let dgram = server.seal(b"echo").unwrap();
        // A server must not accept its own (to-client) packets played back.
        assert!(server.open(&dgram).is_err());
    }
}
