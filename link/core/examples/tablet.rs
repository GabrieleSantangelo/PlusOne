//! A tablet for tests and measurements: it listens, trusts one PC, and
//! serves "echo" (every message back), "sink" (each message's first 8
//! bytes a CLOCK_MONOTONIC time in ns, big-endian: how late it came, told
//! on stdout when the channel ends) and "callback" (each message a feature
//! of the PC's: a channel opened to it, "hello" sent on it).
//!   tablet --cert PEM --key PEM --trust FINGERPRINT [--listen ADDRESS]
//! Its first line on stdout: "listening PORT".

use plusone_link::{Class, Identity, Node};
use std::io::Write;
use std::sync::Arc;

fn monotonic_ns() -> i64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let identity = Identity::from_pem_files(&value("--cert").expect("--cert"), &value("--key").expect("--key")).unwrap();
    let trusted = value("--trust").expect("--trust");
    let node = Node::new("Test tablet", &identity, Arc::new(move |fp: &str| fp == trusted)).unwrap();
    node.serve("echo", 1, |incoming| {
        let mut buf = vec![0u8; 1 << 20];
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 || incoming.channel.send(&buf[..n]).is_err() {
                break;
            }
        }
    });
    node.serve("sink", 1, |incoming| {
        let mut buf = vec![0u8; 1 << 20];
        let mut late = Vec::new();
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
            if n >= 8 && incoming.channel.class != Class::Control {
                late.push(monotonic_ns() - i64::from_be_bytes(buf[..8].try_into().unwrap()));
            }
        }
        late.sort_unstable();
        let at = |q: f64| late.get(((late.len() as f64 * q) as usize).min(late.len().saturating_sub(1))).copied().unwrap_or(0) as f64 / 1e6;
        println!("sink {}: {} came; p50 {:.3} ms, p99 {:.3}, p99.9 {:.3}, max {:.3}", incoming.channel.name, late.len(),
                 at(0.5), at(0.99), at(0.999), late.last().copied().unwrap_or(0) as f64 / 1e6);
        let _ = std::io::stdout().flush();
    });
    node.serve("callback", 1, |incoming| {
        let mut buf = vec![0u8; 4096];
        let mut opened = Vec::new();
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
            let feature = String::from_utf8_lossy(&buf[..n]).into_owned();
            match incoming.session.open(&feature, "back", Class::Control) {
                Ok(channel) => {
                    let _ = channel.send(b"hello");
                    opened.push(channel);
                    let _ = incoming.channel.send(b"opened");
                }
                Err(e) => {
                    let _ = incoming.channel.send(e.as_bytes());
                }
            }
        }
    });
    let bound = node.listen(value("--listen").unwrap_or_else(|| "127.0.0.1:0".into()).parse().unwrap()).unwrap();
    println!("listening {}", bound.port());
    let _ = std::io::stdout().flush();
    loop {
        std::thread::park();
    }
}
