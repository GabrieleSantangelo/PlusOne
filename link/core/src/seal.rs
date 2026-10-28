//! Datagrams sealed with AES-256-GCM. In the clear, the nonce: the
//! direction and a counter; sealed, the channel and the bytes. The keys
//! come from the session's TLS exporter, one per direction.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use std::sync::atomic::{AtomicU64, Ordering};

const NONCE: usize = 12;
const TAG: usize = 16;
/// A datagram's bytes besides the channel's own.
pub const OVERHEAD: usize = NONCE + 2 + TAG;
/// From the side that connected, and from the side that listened.
pub const DIALER: u32 = 1;
pub const LISTENER: u32 = 2;
/// How far behind the newest a datagram may come and still count
/// (Wi-Fi reorders; a reordered packet is not a replay).
const WINDOW: u64 = 1024;

fn key(bytes: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, bytes).expect("a 32-byte key"))
}

fn nonce(direction: u32, count: u64) -> [u8; NONCE] {
    let mut nonce = [0u8; NONCE];
    nonce[..4].copy_from_slice(&direction.to_be_bytes());
    nonce[4..].copy_from_slice(&count.to_be_bytes());
    nonce
}

/// Our side's datagrams: any thread seals (the counter is atomic).
pub struct Sealer {
    key: LessSafeKey,
    direction: u32,
    sent: AtomicU64,
}

impl Sealer {
    pub fn new(key_bytes: &[u8; 32], direction: u32) -> Sealer {
        Sealer { key: key(key_bytes), direction, sent: AtomicU64::new(0) }
    }

    /// out: the datagram (what it had is gone).
    pub fn seal(&self, channel: u16, body: &[u8], out: &mut Vec<u8>) {
        let nonce = nonce(self.direction, self.sent.fetch_add(1, Ordering::Relaxed));
        out.clear();
        out.reserve(OVERHEAD + body.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&channel.to_be_bytes());
        out.extend_from_slice(body);
        let tag = self.key.seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut out[NONCE..])
            .expect("sealing");
        out.extend_from_slice(tag.as_ref());
    }
}

/// The other side's datagrams, each once.
pub struct Opener {
    key: LessSafeKey,
    direction: u32,
    newest: Option<u64>,
    seen: [u64; (WINDOW / 64) as usize],
}

impl Opener {
    pub fn new(key_bytes: &[u8; 32], direction: u32) -> Opener {
        Opener { key: key(key_bytes), direction, newest: None, seen: [0; (WINDOW / 64) as usize] }
    }

    /// (its counter, its channel, its bytes), or None: not ours, tampered
    /// with, too old, or seen already.
    pub fn open<'a>(&mut self, packet: &'a mut [u8]) -> Option<(u64, u16, &'a [u8])> {
        if packet.len() < OVERHEAD {
            return None;
        }
        let (head, sealed) = packet.split_at_mut(NONCE);
        if u32::from_be_bytes([head[0], head[1], head[2], head[3]]) != self.direction {
            return None;
        }
        let count = u64::from_be_bytes(head[4..NONCE].try_into().unwrap());
        if !self.fresh(count) {
            return None;
        }
        let nonce: [u8; NONCE] = (&*head).try_into().unwrap();
        let plain = self.key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), sealed).ok()?;
        self.mark(count);
        Some((count, u16::from_be_bytes([plain[0], plain[1]]), &plain[2..]))
    }

    fn bit(count: u64) -> (usize, u64) {
        let index = count % WINDOW;
        ((index / 64) as usize, 1 << (index % 64))
    }

    fn fresh(&self, count: u64) -> bool {
        match self.newest {
            None => true,
            Some(newest) if count > newest => true,
            Some(newest) => {
                let (word, bit) = Self::bit(count);
                newest - count < WINDOW && self.seen[word] & bit == 0
            }
        }
    }

    fn mark(&mut self, count: u64) {
        match self.newest {
            None => {
                self.newest = Some(count);
            }
            Some(newest) if count > newest => {
                // The counts the window moves past, forgotten.
                for passed in (newest + 1).max(count.saturating_sub(WINDOW - 1))..=count {
                    let (word, bit) = Self::bit(passed);
                    self.seen[word] &= !bit;
                }
                self.newest = Some(count);
            }
            _ => {}
        }
        let (word, bit) = Self::bit(count);
        self.seen[word] |= bit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Sealer, Opener) {
        (Sealer::new(&[7; 32], DIALER), Opener::new(&[7; 32], DIALER))
    }

    #[test]
    fn sealed_and_opened() {
        let (sealer, mut opener) = pair();
        let mut packet = Vec::new();
        sealer.seal(5, b"hello", &mut packet);
        assert_eq!(packet.len(), OVERHEAD + 5);
        let (count, channel, body) = opener.open(&mut packet).unwrap();
        assert_eq!((count, channel, body), (0, 5, &b"hello"[..]));
    }

    #[test]
    fn tampered_or_reflected_refused() {
        let (sealer, mut opener) = pair();
        let mut packet = Vec::new();
        sealer.seal(1, b"x", &mut packet);
        let mut tampered = packet.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(opener.open(&mut tampered).is_none());
        // Our own packets sent back to us: the other direction.
        let mut ours = Opener::new(&[7; 32], LISTENER);
        assert!(ours.open(&mut packet.clone()).is_none());
        assert!(opener.open(&mut packet).is_some());
    }

    #[test]
    fn replays_refused_reordered_kept() {
        let (sealer, mut opener) = pair();
        let packets: Vec<Vec<u8>> = (0..2000).map(|i| {
            let mut p = Vec::new();
            sealer.seal(1, &[i as u8], &mut p);
            p
        }).collect();
        let mut open = |i: usize| opener.open(&mut packets[i].clone()).map(|(c, _, _)| c);
        assert_eq!(open(10), Some(10));
        assert_eq!(open(3), Some(3));    // late, within the window
        assert_eq!(open(3), None);       // again
        assert_eq!(open(10), None);
        assert_eq!(open(1500), Some(1500));
        assert_eq!(open(11), None);      // too far behind now
        assert_eq!(open(1499), Some(1499));
        assert_eq!(open(477), Some(477)); // 1023 behind: still in
        assert_eq!(open(476), None);
    }
}
