//! A stream's bytes cut anywhere: frames out, never a panic, never one
//! bigger than frame::MAX, and the frames are the bytes they came from.
#![no_main]

use libfuzzer_sys::fuzz_target;
use plusone_link::frame::{encode, Decoder, MAX};

fuzz_target!(|data: &[u8]| {
    let Some((&cut, stream)) = data.split_first() else { return };
    let mut decoder = Decoder::default();
    let mut again = Vec::new();
    for piece in stream.chunks(cut as usize + 1) {
        decoder.push(piece);
        loop {
            match decoder.next() {
                Ok(Some((kind, channel, body))) => {
                    assert!(body.len() <= MAX);
                    encode(kind, channel, &body, &mut again);
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
    assert!(stream.starts_with(&again), "frames that were not the stream's");
});
