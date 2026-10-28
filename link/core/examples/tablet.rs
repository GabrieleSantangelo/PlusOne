//! A tablet for tests and measurements: it listens, trusts one PC, and
//! serves "echo" (every message back), "sink" (each message's first 8
//! bytes a CLOCK_MONOTONIC time in ns, big-endian: how late it came, told
//! on stdout when the channel ends) and "callback" (each message a feature
//! of the PC's: a channel opened to it, "hello" sent on it) and "frames"
//! (a realtime channel's frames in pieces: frame u32, index u16, count
//! u16, the rest anything; each frame whole answered with its number).
//!   tablet --cert PEM --key PEM --trust FINGERPRINT [--listen ADDRESS] [--quic]
//! Its first line on stdout: "listening PORT"; with --quic it takes QUIC on
//! the same port (UDP) too.

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
    node.serve("frames", 1, |incoming| {
        let mut buf = vec![0u8; 1 << 16];
        let mut frames: std::collections::HashMap<u32, (u16, u16)> = std::collections::HashMap::new();
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
            if n < 8 {
                continue;
            }
            let frame = u32::from_be_bytes(buf[..4].try_into().unwrap());
            let count = u16::from_be_bytes(buf[6..8].try_into().unwrap());
            let entry = frames.entry(frame).or_insert((count, 0));
            entry.1 += 1;
            if entry.1 == entry.0 {
                frames.remove(&frame);
                let _ = incoming.channel.send(&frame.to_be_bytes());
            }
            frames.retain(|f, _| f.wrapping_add(240) > frame);
        }
    });
    let bound = node.listen(value("--listen").unwrap_or_else(|| "127.0.0.1:0".into()).parse().unwrap()).unwrap();
    println!("listening {}", bound.port());
    if args.iter().any(|a| a == "--quic") {
        let quic = node.listen_quic(bound).unwrap();
        println!("quic {}", quic.port());
    }
    let _ = std::io::stdout().flush();
    loop {
        std::thread::park();
    }
}
