//! The link on loopback with every channel at once, as the tablet is used:
//! video (1200-byte packets, 20 Mbit/s in 60 bursts a second), sound (a
//! packet each 5 ms), input (285 packets a second), and control round
//! trips. Each packet's time from the feature's write to the other
//! feature's read.
//!   tools/link-docker.sh run --release --example bench [-- SECONDS]

use plusone_link::{Class, Identity, Node};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn identity() -> (Identity, String) {
    let made = rcgen::generate_simple_self_signed(vec!["plus-one".into()]).unwrap();
    let identity = Identity::new(made.cert.der().to_vec(), made.key_pair.serialize_der()).unwrap();
    let fingerprint = identity.fingerprint();
    (identity, fingerprint)
}

fn describe(name: &str, mut ns: Vec<u64>, sent: usize) {
    ns.sort_unstable();
    let at = |q: f64| ns.get(((ns.len() as f64 * q) as usize).min(ns.len().saturating_sub(1))).copied().unwrap_or(0) as f64 / 1e6;
    println!("{name:>8}: {} of {sent}; p50 {:.3} ms, p99 {:.3}, p99.9 {:.3}, max {:.3}",
             ns.len(), at(0.5), at(0.99), at(0.999), ns.last().copied().unwrap_or(0) as f64 / 1e6);
}

fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    now_ns();
    let (pc_id, pc_fp) = identity();
    let (tablet_id, tablet_fp) = identity();
    let tablet = Node::new("tablet", &tablet_id, Arc::new(move |f: &str| f == pc_fp)).unwrap();
    let (results, collected) = mpsc::channel::<(String, Vec<u64>)>();
    tablet.serve("bench", 1, move |incoming| {
        let results = results.clone();
        let mut buf = vec![0u8; 1 << 18];
        let mut latencies = Vec::new();
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
            if incoming.channel.class == Class::Control {
                let _ = incoming.channel.send(&buf[..n]);  // a round trip
                continue;
            }
            latencies.push(now_ns() - u64::from_be_bytes(buf[..8].try_into().unwrap()));
        }
        let _ = results.send((incoming.channel.name.clone(), latencies));
    });
    let address = tablet.listen("127.0.0.1:0".parse().unwrap()).unwrap();
    let pc = Node::new("pc", &pc_id, Arc::new(move |f: &str| f == tablet_fp)).unwrap();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    while session.rtt().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
    let until = Instant::now() + Duration::from_secs(seconds);
    // (name, class, packets per burst, size, bursts a second)
    let flows = [("video", Class::Realtime, 30usize, 1200usize, 60u64), ("sound", Class::Realtime, 1, 980, 200),
                 ("input", Class::Input, 1, 40, 285)];
    let mut senders = Vec::new();
    for (name, class, burst, size, rate) in flows {
        let channel = session.open("bench", name, class).unwrap();
        senders.push(std::thread::spawn(move || {
            let mut packet = vec![0u8; size];
            let mut sent = 0;
            let mut next = Instant::now();
            while Instant::now() < until {
                for _ in 0..burst {
                    packet[..8].copy_from_slice(&now_ns().to_be_bytes());
                    channel.send(&packet).unwrap();
                    sent += 1;
                }
                next += Duration::from_nanos(1_000_000_000 / rate);
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
            std::thread::sleep(Duration::from_millis(200));  // the last ones in
            drop(channel);
            (name, sent)
        }));
    }
    // Control: a round trip every 10 ms, alongside.
    let control = session.open("bench", "control", Class::Control).unwrap();
    let mut round_trips = Vec::new();
    let mut buf = [0u8; 64];
    while Instant::now() < until {
        let start = now_ns();
        control.send(&start.to_be_bytes()).unwrap();
        if let Ok(Some(_)) = control.recv(&mut buf, Some(Duration::from_secs(1))) {
            round_trips.push(now_ns() - start);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let sent: Vec<(&str, usize)> = senders.into_iter().map(|s| s.join().unwrap()).collect();
    drop(control);
    println!("{seconds} s on loopback, every channel at once (one way, the feature's write to the other's read):");
    for _ in 0..sent.len() {
        let (name, latencies) = collected.recv_timeout(Duration::from_secs(5)).unwrap();
        let count = sent.iter().find(|(n, _)| *n == name).map(|(_, c)| *c).unwrap_or(0);
        describe(&name, latencies, count);
    }
    let trips = round_trips.len();
    describe("control", round_trips, trips);
    println!("dropped: {}", session.dropped());
}
