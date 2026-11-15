//! What the app advertises over Bluetooth LE for the PCs it is paired with,
//! where multicast does not go (eduroam, a guest network) and the last
//! address no longer holds: its own address on the network it is on, which
//! only that PC can read. The same in the app (BleBlocks.kt):
//!
//! ```text
//!     manufacturer data, company COMPANY: VERSION, then a block for each PC (two at most)
//!     block  = nonce (1) ‖ ciphertext (6: IPv4 ‖ port, big endian) ‖ mac (5)
//!     key    = HMAC key SHA-256("plus-one ble 1" ‖ app's fingerprint ‖ PC's fingerprint)
//!     stream = HMAC(key, "k" ‖ slot (8 BE) ‖ nonce)[..6]; ciphertext = plain ⊕ stream
//!     mac    = HMAC(key, "m" ‖ slot (8 BE) ‖ nonce ‖ ciphertext)[..5]
//!     slot   = Unix time / SLOT
//! ```
//!
//! Nobody else can tell whose a block is, nor read it, nor follow the tablet
//! from one slot to the next; a nonce keeps two addresses in one slot from
//! giving away what they differ by. It is only a hint: the PC dials the
//! address, and who answers is checked by its certificate.

use ring::{digest, hmac};
use std::net::{Ipv4Addr, SocketAddr};

/// The company identifier of the manufacturer data (0xFFFF: the one Bluetooth
/// leaves for what is not a company's).
pub const COMPANY: u16 = 0xFFFF;
pub const VERSION: u8 = 1;
/// A quarter of an hour, as the mDNS tags (mdns::TAG_EPOCH).
pub const SLOT: u64 = 900;
pub const BLOCK: usize = 12;
/// The first byte of the advertisement of a tablet whose user holds pairing open.
pub const PAIRING: u8 = 2;
/// The service data of its scan response (16-bit UUID 0xFFFF) holds its name.
pub const NAME_UUID: &str = "0000ffff-0000-1000-8000-00805f9b34fb";
/// What fits of the name: 31 bytes less the 2 of the header and the 2 of the UUID.
pub const NAME_MOST: usize = 27;

fn bytes(hex: &str) -> Option<Vec<u8>> {
    (0..hex.len() / 2).map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

fn key(app: &str, pc: &str) -> Option<hmac::Key> {
    let mut data = b"plus-one ble 1".to_vec();
    data.extend(bytes(app)?);
    data.extend(bytes(pc)?);
    Some(hmac::Key::new(hmac::HMAC_SHA256, digest::digest(&digest::SHA256, &data).as_ref()))
}

fn part(key: &hmac::Key, label: u8, slot: u64, nonce: u8, ciphertext: &[u8]) -> Vec<u8> {
    let mut message = vec![label];
    message.extend(slot.to_be_bytes());
    message.push(nonce);
    message.extend(ciphertext);
    hmac::sign(key, &message).as_ref().to_vec()
}

/// The block that tells address (IPv4, port) to the PC, in slot: what the app makes.
pub fn seal(app: &str, pc: &str, slot: u64, nonce: u8, address: (Ipv4Addr, u16)) -> Option<[u8; BLOCK]> {
    let key = key(app, pc)?;
    let mut plain = address.0.octets().to_vec();
    plain.extend(address.1.to_be_bytes());
    let stream = part(&key, b'k', slot, nonce, &[]);
    let ciphertext: Vec<u8> = plain.iter().zip(&stream).map(|(p, s)| p ^ s).collect();
    let mac = part(&key, b'm', slot, nonce, &ciphertext);
    let mut block = [0u8; BLOCK];
    block[0] = nonce;
    block[1..7].copy_from_slice(&ciphertext);
    block[7..].copy_from_slice(&mac[..BLOCK - 7]);
    Some(block)
}

/// Equal, in the time of the longer (a mac is not told apart by where it differs).
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |different, (x, y)| different | (x ^ y)) == 0
}

/// The address a block tells, if it is the app's for this PC in slot.
fn open(key: &hmac::Key, slot: u64, block: &[u8]) -> Option<SocketAddr> {
    let (nonce, ciphertext, mac) = (block[0], &block[1..7], &block[7..BLOCK]);
    let expected = part(key, b'm', slot, nonce, ciphertext);
    if !same(&expected[..mac.len()], mac) {
        return None;
    }
    let stream = part(key, b'k', slot, nonce, &[]);
    let plain: Vec<u8> = ciphertext.iter().zip(&stream).map(|(c, s)| c ^ s).collect();
    let port = u16::from_be_bytes([plain[4], plain[5]]);
    (port != 0).then(|| SocketAddr::new(Ipv4Addr::new(plain[0], plain[1], plain[2], plain[3]).into(), port))
}

/// The address the manufacturer data (what follows the company identifier)
/// tells this PC, if it is the app's: any block of it, in a slot of now or
/// either side of it (the clocks need not agree).
pub fn read(app: &str, pc: &str, now: u64, data: &[u8]) -> Option<SocketAddr> {
    let key = key(app, pc)?;
    let (version, blocks) = data.split_first()?;
    if *version != VERSION {
        return None;
    }
    let slot = now / SLOT;
    blocks.chunks_exact(BLOCK).find_map(|block| {
        [slot, slot.saturating_sub(1), slot + 1].into_iter().find_map(|slot| open(&key, slot, block))
    })
}

/// A tablet that holds pairing open (Pairing.windowOpen), told in the clear:
/// there is no key between it and a PC yet, as with earphones to be paired.
/// It only says who and where: what makes the pairing is the code both
/// screens show, as ever.
///
/// ```text
///     manufacturer data, company COMPANY: PAIRING ‖ id (8: the start of the app's fingerprint) ‖ IPv4 (4) ‖ port (2, BE)
///     scan response, service data NAME_UUID: its name, UTF-8
/// ```
pub fn pairing_data(app: &str, address: (Ipv4Addr, u16)) -> Option<Vec<u8>> {
    let mut data = vec![PAIRING];
    data.extend(bytes(app.get(..16)?)?);
    data.extend(address.0.octets());
    data.extend(address.1.to_be_bytes());
    Some(data)
}

/// The tablet (its id, 16 hex digits, and where it listens) an advertisement tells, if it is one's.
pub fn read_pairing(data: &[u8]) -> Option<(String, SocketAddr)> {
    if data.len() != 15 || data[0] != PAIRING {
        return None;
    }
    let id = data[1..9].iter().map(|b| format!("{b:02x}")).collect();
    let port = u16::from_be_bytes([data[13], data[14]]);
    (port != 0).then(|| (id, SocketAddr::new(Ipv4Addr::new(data[9], data[10], data[11], data[12]).into(), port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const PC: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    const OTHER: &str = "0102030405060708090a0b0c0d0e0f100102030405060708090a0b0c0d0e0f10";

    fn data(blocks: &[[u8; BLOCK]]) -> Vec<u8> {
        let mut data = vec![VERSION];
        blocks.iter().for_each(|b| data.extend(b));
        data
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_same_block_in_the_app_and_here() {
        // The vector BleBlocksTest.kt has too.
        let block = seal(APP, PC, 1_000, 7, (Ipv4Addr::new(192, 168, 1, 112), 47101)).unwrap();
        assert_eq!(hex(&block), "07e980cedfb1742b53e008ca");
    }

    #[test]
    fn a_block_tells_its_address_to_the_pc_it_is_for() {
        let block = seal(APP, PC, 1_000, 7, (Ipv4Addr::new(192, 168, 1, 112), 47101)).unwrap();
        let now = 1_000 * SLOT + 5;
        assert_eq!(read(APP, PC, now, &data(&[block])), Some("192.168.1.112:47101".parse().unwrap()));
        // Two blocks, the PC's second.
        let other = seal(APP, OTHER, 1_000, 9, (Ipv4Addr::new(10, 0, 0, 2), 1)).unwrap();
        assert_eq!(read(APP, PC, now, &data(&[other, block])), Some("192.168.1.112:47101".parse().unwrap()));
        assert_eq!(read(APP, OTHER, now, &data(&[other, block])), Some("10.0.0.2:1".parse().unwrap()));
    }

    #[test]
    fn no_one_else_reads_it() {
        let block = seal(APP, PC, 1_000, 7, (Ipv4Addr::new(192, 168, 1, 112), 47101)).unwrap();
        let now = 1_000 * SLOT;
        assert_eq!(read(APP, OTHER, now, &data(&[block])), None, "another PC");
        assert_eq!(read(OTHER, PC, now, &data(&[block])), None, "another app");
        let mut altered = block;
        altered[3] ^= 1;
        assert_eq!(read(APP, PC, now, &data(&[altered])), None, "a block changed on the way");
        assert_eq!(read(APP, PC, now, &[2, 0, 0]), None);
        assert_eq!(read(APP, PC, now, &[]), None);
        assert_eq!(read(APP, PC, now, &[VERSION, 1, 2, 3]), None, "a part of a block");
        assert_eq!(read("not hex", PC, now, &data(&[block])), None);
    }

    #[test]
    fn the_slot_may_be_off_by_one_not_more() {
        let block = seal(APP, PC, 1_000, 7, (Ipv4Addr::new(192, 168, 1, 112), 47101)).unwrap();
        for (slot, read_it) in [(999, true), (1_000, true), (1_001, true), (998, false), (1_002, false)] {
            assert_eq!(read(APP, PC, slot * SLOT + 1, &data(&[block])).is_some(), read_it, "slot {slot}");
        }
    }

    #[test]
    fn a_nonce_makes_the_same_address_another_block() {
        let address = (Ipv4Addr::new(192, 168, 1, 112), 47101);
        let (one, two) = (seal(APP, PC, 1_000, 1, address).unwrap(), seal(APP, PC, 1_000, 2, address).unwrap());
        assert_ne!(one[1..7], two[1..7]);
        assert_eq!(read(APP, PC, 1_000 * SLOT, &data(&[two])), read(APP, PC, 1_000 * SLOT, &data(&[one])));
    }

    #[test]
    fn a_port_of_zero_is_no_address() {
        let block = seal(APP, PC, 1_000, 1, (Ipv4Addr::new(192, 168, 1, 112), 0)).unwrap();
        assert_eq!(read(APP, PC, 1_000 * SLOT, &data(&[block])), None);
    }

    #[test]
    fn a_tablet_holding_pairing_open_is_told_in_the_clear() {
        // The vector BleBlocksTest.kt has too.
        let data = pairing_data(APP, (Ipv4Addr::new(192, 168, 1, 112), 47101)).unwrap();
        assert_eq!(hex(&data), "020011223344556677c0a80170b7fd");
        assert_eq!(read_pairing(&data), Some(("0011223344556677".to_owned(), "192.168.1.112:47101".parse().unwrap())));
    }

    #[test]
    fn what_is_not_a_pairing_advertisement_is_none() {
        let data = pairing_data(APP, (Ipv4Addr::new(10, 0, 0, 2), 1)).unwrap();
        assert!(read_pairing(&data[..14]).is_none());
        assert!(read_pairing(&[data.as_slice(), &[0]].concat()).is_none());
        let mut other = data.clone();
        other[0] = VERSION;
        assert!(read_pairing(&other).is_none(), "a sealed block's data");
        assert!(read_pairing(&pairing_data(APP, (Ipv4Addr::new(10, 0, 0, 2), 0)).unwrap()).is_none(), "port 0");
        assert!(pairing_data("0011", (Ipv4Addr::new(10, 0, 0, 2), 1)).is_none(), "an id too short");
    }
}
