//! What reaches a datagram port, from anyone: sealed packets of ours in any
//! order, again and again, cut short or with bits flipped, and bytes of no
//! one's. None may panic; each counter opens once at most; nothing opens
//! that was not sealed so; nothing opens older than the replay window.
#![no_main]

use libfuzzer_sys::fuzz_target;
use plusone_link::seal::{Opener, Sealer};
use std::sync::OnceLock;

const KEY: [u8; 32] = [7; 32];
const DIRECTION: u32 = 1;
/// More than the replay window (1024), so some are too old.
const SEALED: usize = 3000;

fn sealed() -> &'static Vec<Vec<u8>> {
    static SEALED_ONCE: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    SEALED_ONCE.get_or_init(|| {
        let sealer = Sealer::new(&KEY, DIRECTION);
        (0..SEALED).map(|i| {
            let mut out = Vec::new();
            sealer.seal((i % 7) as u16, &(i as u32).to_be_bytes(), &mut out);
            out
        }).collect()
    })
}

fuzz_target!(|data: &[u8]| {
    let mut opener = Opener::new(&KEY, DIRECTION);
    let mut opened = std::collections::HashSet::new();
    let mut newest: Option<u64> = None;
    // Each 4 bytes: which packet (2), what is done to it (1), where (1).
    for step in data.chunks_exact(4) {
        let index = u16::from_be_bytes([step[0], step[1]]) as usize % SEALED;
        let mut packet = sealed()[index].clone();
        let tampered = match step[2] % 4 {
            0 => false,
            1 => {
                let at = step[3] as usize % packet.len();
                packet[at] ^= 1 << (step[2] >> 5);
                true
            }
            2 => {
                packet.truncate(step[3] as usize % packet.len());
                true
            }
            _ => {
                packet = step.repeat(step[3] as usize % 64);
                true
            }
        };
        if let Some((count, channel, body)) = opener.open(&mut packet) {
            assert!(!tampered, "a tampered packet opened");
            assert_eq!(count as usize, index);
            assert_eq!(channel as usize, index % 7);
            assert_eq!(body, (index as u32).to_be_bytes());
            assert!(opened.insert(count), "{count} opened twice");
            assert!(newest.is_none_or(|n| count + 1024 > n), "{count} opened past the window");
            newest = Some(newest.map_or(count, |n| n.max(count)));
        }
    }
});
