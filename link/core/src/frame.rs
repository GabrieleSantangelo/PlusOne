//! Frames on a stream (the session's TLS stream; later a USB or a
//! Bluetooth one): a kind, a channel, a length, the bytes.

use std::io;

/// A message of the session's own (JSON): hello, open, accept, ping...
pub const SESSION: u8 = 0;
/// A control channel's message.
pub const MESSAGE: u8 = 1;
/// An input or realtime channel's packet, while there is no datagram path.
pub const DATAGRAM: u8 = 2;

pub const HEADER: usize = 7;
/// The largest frame: a control message up to this.
pub const MAX: usize = 1 << 20;

pub fn encode(kind: u8, channel: u16, body: &[u8], out: &mut Vec<u8>) {
    out.reserve(HEADER + body.len());
    out.push(kind);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
}

/// Frames out of a stream's bytes, which its reads split anywhere.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    at: usize,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        if self.at > 0 && self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// The next whole frame: (kind, channel, body); an error if it is too big.
    pub fn next(&mut self) -> io::Result<Option<(u8, u16, Vec<u8>)>> {
        let rest = &self.buf[self.at..];
        if rest.len() < HEADER {
            self.compact();
            return Ok(None);
        }
        let length = u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]) as usize;
        if length > MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a frame of {length} bytes")));
        }
        if rest.len() < HEADER + length {
            self.compact();
            return Ok(None);
        }
        let frame = (rest[0], u16::from_be_bytes([rest[1], rest[2]]), rest[HEADER..HEADER + length].to_vec());
        self.at += HEADER + length;
        Ok(Some(frame))
    }

    /// What was taken out, dropped from the front (once, not per frame).
    fn compact(&mut self) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_anywhere() {
        let mut bytes = Vec::new();
        encode(SESSION, 0, b"{\"t\":\"ping\"}", &mut bytes);
        encode(DATAGRAM, 7, &[1, 2, 3], &mut bytes);
        encode(MESSAGE, 3, b"", &mut bytes);
        for cut in 0..bytes.len() {
            let mut decoder = Decoder::default();
            let mut frames = Vec::new();
            for part in [&bytes[..cut], &bytes[cut..]] {
                decoder.push(part);
                while let Some(frame) = decoder.next().unwrap() {
                    frames.push(frame);
                }
            }
            assert_eq!(frames, vec![(SESSION, 0, b"{\"t\":\"ping\"}".to_vec()), (DATAGRAM, 7, vec![1, 2, 3]),
                                    (MESSAGE, 3, vec![])]);
        }
    }

    #[test]
    fn too_big_refused() {
        let mut decoder = Decoder::default();
        decoder.push(&[MESSAGE, 0, 1, 0xff, 0xff, 0xff, 0xff]);
        assert!(decoder.next().is_err());
    }
}
