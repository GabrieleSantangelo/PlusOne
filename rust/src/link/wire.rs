//! Framed messages over a stream: a 4-byte big-endian length, a JSON header,
//! and as many payload bytes as the header's "size" says (wire.py, Wire.kt).

use serde_json::{Map, Value};
use std::io::{self, Read, Write};

pub const MAX_HEADER: usize = 16 * 1024 * 1024;  // the marks of a long document (the app takes as much)
pub const UNPAIRED_HEADER: usize = 4096;         // hello and pairing: small, from an app not yet known

pub type Message = Map<String, Value>;

/// A message as a JSON object: `msg!({"type": "ping"})`.
#[macro_export]
macro_rules! msg {
    ($($json:tt)+) => {
        match serde_json::json!($($json)+) {
            serde_json::Value::Object(map) => map,
            _ => unreachable!("a message is an object"),
        }
    };
}

/// One message, its payload after it, written at once.
pub fn send<W: Write>(stream: &mut W, header: &Message, payload: Option<&[u8]>) -> io::Result<()> {
    let mut header = header.clone();
    if let Some(payload) = payload {
        header.insert("size".into(), Value::from(payload.len()));
    }
    let data = serde_json::to_vec(&header).map_err(io::Error::other)?;
    let mut out = Vec::with_capacity(4 + data.len() + payload.map_or(0, <[u8]>::len));
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&data);
    if let Some(payload) = payload {
        out.extend_from_slice(payload);
    }
    stream.write_all(&out)?;
    stream.flush()
}

/// The next message (the app sends headers only), at most limit bytes.
pub fn receive<R: Read>(stream: &mut R, limit: usize) -> io::Result<Message> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad header length {length}")));
    }
    let mut data = vec![0u8; length];
    stream.read_exact(&mut data)?;
    match serde_json::from_slice::<Value>(&data) {
        Ok(Value::Object(header)) => {
            if header.get("size").is_some_and(|s| s.as_u64().unwrap_or(1) != 0) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "the app sends no payloads"));
            }
            Ok(header)
        }
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "a message that is not an object")),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_and_its_payload_go_as_wire_py_sends_them() {
        let mut out = Vec::new();
        send(&mut out, &msg!({"type": "sign", "job": "a"}), Some(b"%PDF")).unwrap();
        let length = u32::from_be_bytes(out[..4].try_into().unwrap()) as usize;
        let header: Value = serde_json::from_slice(&out[4..4 + length]).unwrap();
        assert_eq!(header["size"], 4);
        assert_eq!(&out[4 + length..], b"%PDF");
    }

    #[test]
    fn what_the_app_sends_is_read_and_checked() {
        let mut data = Vec::new();
        send(&mut data, &msg!({"type": "pong"}), None).unwrap();
        assert_eq!(receive(&mut data.as_slice(), MAX_HEADER).unwrap()["type"], "pong");
        assert!(receive(&mut data.as_slice(), 3).is_err());  // longer than allowed
        let mut sized = Vec::new();
        send(&mut sized, &msg!({"type": "x", "size": 3}), None).unwrap();
        assert!(receive(&mut sized.as_slice(), MAX_HEADER).is_err());  // no payloads from the app
        let mut list = 2u32.to_be_bytes().to_vec();
        list.extend_from_slice(b"[]");
        assert!(receive(&mut list.as_slice(), MAX_HEADER).is_err());
    }
}
