//! A relay: the last path, when the two sides cannot reach each other on
//! any network (each behind its own, or on one that keeps clients apart).
//! Each dials it and says a rendezvous (from the two devices' fingerprints,
//! so both know it and no one else can tell whose it is); the relay joins
//! the first two that say the same, and copies bytes between them, as they
//! are: the session's TLS goes through it end to end, the relay sees no
//! more than lengths and times. No relay server is run by the project yet:
//! serve() is what one runs (and what the tests run locally).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::time::Duration;

const HELLO: &str = "PLUSONE-RELAY-1 ";
const PAIRED: &[u8] = b"PAIRED\n";
/// A side waits this long for the other.
const WAIT: Duration = Duration::from_secs(30);

/// The rendezvous of two devices (their fingerprints, in either order).
pub fn rendezvous(one: &str, other: &str) -> String {
    let (a, b) = if one <= other { (one, other) } else { (other, one) };
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("plus-one relay/{a}/{b}").as_bytes());
    digest.as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// A stream to the other side through the relay at address, once both are
/// there (WAIT at most).
pub fn dial(address: SocketAddr, rendezvous: &str, timeout: Duration) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    crate::stream::tune_tcp(&stream);
    stream.write_all(format!("{HELLO}{rendezvous}\n").as_bytes())?;
    stream.set_read_timeout(Some(WAIT))?;
    let mut answer = [0u8; PAIRED.len()];
    stream.read_exact(&mut answer)?;
    if answer != PAIRED {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a relay"));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// A relay on listener: pairs and copies, each pair on threads of its own.
pub fn serve(listener: TcpListener) {
    let waiting: Arc<Mutex<HashMap<String, TcpStream>>> = Arc::default();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let waiting = waiting.clone();
        let _ = std::thread::Builder::new().name("relay-hello".into()).spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut line = String::new();
            let Ok(reader) = stream.try_clone() else { return };
            if BufReader::new(reader).take(64).read_line(&mut line).is_err() {
                return;
            }
            let Some(id) = line.trim_end().strip_prefix(HELLO).filter(|id| id.len() == 32).map(str::to_owned) else {
                return;
            };
            let _ = stream.set_read_timeout(None);
            let other = waiting.lock().unwrap().remove(&id);
            match other {
                Some(other) => join(other, stream),
                None => {
                    waiting.lock().unwrap().insert(id, stream);
                }
            }
        });
    }
}

fn join(mut one: TcpStream, mut other: TcpStream) {
    if one.write_all(PAIRED).is_err() || other.write_all(PAIRED).is_err() {
        return;
    }
    let (Ok(one_back), Ok(other_back)) = (one.try_clone(), other.try_clone()) else { return };
    let copy = |mut from: TcpStream, mut to: TcpStream| move || {
        let _ = io::copy(&mut from, &mut to);
        let _ = to.shutdown(Shutdown::Write);
    };
    let _ = std::thread::Builder::new().name("relay-copy".into()).spawn(copy(one_back, other_back));
    copy(other, one)();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_rendezvous_both_ways_and_no_one_elses() {
        assert_eq!(rendezvous("aa", "bb"), rendezvous("bb", "aa"));
        assert_ne!(rendezvous("aa", "bb"), rendezvous("aa", "cc"));
        assert_eq!(rendezvous("aa", "bb").len(), 32);
    }
}
