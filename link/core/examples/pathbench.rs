//! The link's numbers on one path, from the PC's side, against
//! examples/tablet (tools/netbench.sh runs both on an emulated network):
//! realtime round trips idle and under a transfer both ways, a transfer one
//! way and both ways, and a video's frames (60 a second, a keyframe of four
//! every two seconds, in MAX_DATAGRAM pieces): how many came whole, and how
//! soon each was acknowledged; at a fixed bitrate, or following the
//! session's rate (a QUIC path's controller), and at a fixed 4 Mbit/s on
//! both transports; input (285 states a second) and sound (a packet each
//! 5 ms: its gaps) under a transfer; channels opened while one runs (how
//! long, how many never were). One line a test, key=value.
//!   pathbench --cert PEM --key PEM --trust FINGERPRINT --tablet ADDRESS
//!             [--quic] [--seconds S] [--video KBPS] [--opens N]

use plusone_link::session::MAX_DATAGRAM;
use plusone_link::{Class, Identity, Node, Session};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn quantiles(mut ms: Vec<f64>) -> String {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| ms.get(((ms.len() as f64 * q) as usize).min(ms.len().saturating_sub(1))).copied().unwrap_or(f64::NAN);
    format!("p50={:.1} p90={:.1} p99={:.1}", at(0.5), at(0.9), at(0.99))
}

/// Round trips of 1000-byte realtime messages every 5 ms, for seconds.
fn realtime(session: &Arc<Session>, seconds: f64) -> String {
    let channel = session.open("echo", "rt", Class::Realtime).unwrap();
    let reader = channel.try_clone().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let collecting = {
        let done = done.clone();
        std::thread::spawn(move || {
            let (mut buf, mut rtts) = (vec![0u8; 2048], Vec::new());
            while !done.load(Ordering::SeqCst) {
                if let Ok(Some(n)) = reader.recv(&mut buf, Some(Duration::from_millis(100))) {
                    if n >= 8 {
                        rtts.push((now_ns() - u64::from_be_bytes(buf[..8].try_into().unwrap())) as f64 / 1e6);
                    }
                }
            }
            rtts
        })
    };
    let (start, mut sent) = (Instant::now(), 0);
    while start.elapsed().as_secs_f64() < seconds {
        let mut packet = now_ns().to_be_bytes().to_vec();
        packet.resize(1000, 0);
        if channel.send(&packet).is_ok() {
            sent += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(600));
    done.store(true, Ordering::SeqCst);
    let rtts = collecting.join().unwrap();
    format!("back={} sent={sent} {}", rtts.len(), quantiles(rtts))
}

/// Round trips of small messages on a class, every period, for seconds:
/// (round trips in ms, sent, the longest run of numbers that never came).
fn flow(session: &Arc<Session>, class: Class, size: usize, period: Duration, seconds: f64) -> (Vec<f64>, u32, u32) {
    let channel = session.open("echo", "flow", class).unwrap();
    let reader = channel.try_clone().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let collecting = {
        let done = done.clone();
        std::thread::spawn(move || {
            let (mut buf, mut rtts, mut seen) = (vec![0u8; 2048], Vec::new(), Vec::new());
            while !done.load(Ordering::SeqCst) {
                if let Ok(Some(n)) = reader.recv(&mut buf, Some(Duration::from_millis(100))) {
                    if n >= 12 {
                        rtts.push((now_ns() - u64::from_be_bytes(buf[..8].try_into().unwrap())) as f64 / 1e6);
                        seen.push(u32::from_be_bytes(buf[8..12].try_into().unwrap()));
                    }
                }
            }
            (rtts, seen)
        })
    };
    let (start, mut number) = (Instant::now(), 0u32);
    while start.elapsed().as_secs_f64() < seconds {
        let mut message = now_ns().to_be_bytes().to_vec();
        message.extend_from_slice(&number.to_be_bytes());
        message.resize(size.max(12), 0);
        let _ = channel.send(&message);
        number += 1;
        std::thread::sleep(period);
    }
    std::thread::sleep(Duration::from_millis(600));
    done.store(true, Ordering::SeqCst);
    let (rtts, mut seen) = collecting.join().unwrap();
    seen.sort_unstable();
    seen.dedup();
    // The longest gap: numbers in a row that never came (a sound's dropout).
    let mut gap = 0u32;
    let mut previous: Option<u32> = None;
    for n in seen.iter().copied().chain(std::iter::once(number)) {
        let missing = match previous {
            Some(p) => n.saturating_sub(p + 1),
            None => n,
        };
        gap = gap.max(missing);
        previous = Some(n);
    }
    (rtts, number, gap)
}

/// Channels opened one after the other while an upload runs: each opened,
/// a message there and back on it; (ms each, how many never answered).
fn opens(session: &Arc<Session>, count: usize) -> (Vec<f64>, usize) {
    let (mut times, mut failed) = (Vec::new(), 0);
    let mut buf = vec![0u8; 64];
    for i in 0..count {
        let begun = Instant::now();
        let answered = session.open("echo", &format!("open{i}"), Class::Control).ok().and_then(|channel| {
            channel.send(b"open").ok()?;
            channel.recv(&mut buf, Some(Duration::from_secs(5))).ok().flatten()
        });
        match answered {
            Some(4) => times.push(begun.elapsed().as_secs_f64() * 1000.0),
            _ => failed += 1,
        }
    }
    (times, failed)
}

/// A transfer for seconds: to the sink (one way), or to the echo (both
/// ways, counted as it comes back): MB/s.
fn transfer(session: &Arc<Session>, feature: &str, seconds: f64) -> f64 {
    let channel = session.open(feature, "bulk", Class::Control).unwrap();
    let returned = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let reading = (feature == "echo").then(|| {
        let (reader, returned, done) = (channel.try_clone().unwrap(), returned.clone(), done.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 17];
            while !done.load(Ordering::SeqCst) {
                if let Ok(Some(n)) = reader.recv(&mut buf, Some(Duration::from_millis(100))) {
                    returned.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        })
    });
    let (start, piece, mut sent) = (Instant::now(), vec![7u8; 64 << 10], 0u64);
    while start.elapsed().as_secs_f64() < seconds {
        if channel.send(&piece).is_err() {
            break;
        }
        sent += piece.len() as u64;
    }
    let elapsed = start.elapsed().as_secs_f64();
    done.store(true, Ordering::SeqCst);
    if let Some(reading) = reading {
        let _ = reading.join();
        return returned.load(Ordering::Relaxed) as f64 / elapsed / 1e6;
    }
    // One way: what was sent less a window still on its way.
    sent.saturating_sub(1 << 20) as f64 / elapsed / 1e6
}

/// A video for seconds at kbps, or (None) at 0.85 of the session's rate.
fn video(session: &Arc<Session>, seconds: f64, kbps: Option<u64>) -> String {
    let channel = session.open("frames", "v", Class::Realtime).unwrap();
    let reader = channel.try_clone().unwrap();
    let sent_at: Arc<Mutex<HashMap<u32, u64>>> = Arc::default();
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let collecting = {
        let (sent_at, latencies, done) = (sent_at.clone(), latencies.clone(), done.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64];
            while !done.load(Ordering::SeqCst) {
                if let Ok(Some(4)) = reader.recv(&mut buf, Some(Duration::from_millis(100))) {
                    let frame = u32::from_be_bytes(buf[..4].try_into().unwrap());
                    if let Some(at) = sent_at.lock().unwrap().remove(&frame) {
                        latencies.lock().unwrap().push((now_ns() - at) as f64 / 1e6);
                    }
                }
            }
        })
    };
    let payload = MAX_DATAGRAM - 8;
    let (start, mut frame, mut bytes, mut rates) = (Instant::now(), 0u32, 0u64, Vec::new());
    while start.elapsed().as_secs_f64() < seconds {
        let due = start + Duration::from_secs_f64(frame as f64 / 60.0);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let rate = match kbps {
            Some(kbps) => kbps * 1000 / 8,
            None => session.rate().map(|r| r * 85 / 100).unwrap_or(250_000).clamp(12_500, 2_500_000),
        };
        rates.push(rate);
        let average = (rate / 60) as usize;
        let size = if frame % 120 == 0 { average * 4 } else { average }.max(1);
        let count = size.div_ceil(payload);
        sent_at.lock().unwrap().insert(frame, now_ns());
        for index in 0..count {
            let mut packet = frame.to_be_bytes().to_vec();
            packet.extend_from_slice(&(index as u16).to_be_bytes());
            packet.extend_from_slice(&(count as u16).to_be_bytes());
            packet.resize(8 + payload.min(size - index * payload), 0);
            bytes += packet.len() as u64;
            let _ = channel.send(&packet);
        }
        frame += 1;
    }
    std::thread::sleep(Duration::from_millis(800));
    done.store(true, Ordering::SeqCst);
    let _ = collecting.join();
    let whole = latencies.lock().unwrap().len();
    let mean = rates.iter().sum::<u64>() / rates.len().max(1) as u64;
    format!("whole={:.1}% kbps={} target={} {}", 100.0 * whole as f64 / frame.max(1) as f64,
            (bytes as f64 * 8.0 / 1000.0 / seconds) as u64, mean * 8 / 1000, quantiles(latencies.lock().unwrap().clone()))
}

/// Until what the last test left on its way is gone (each test measures
/// itself, not the one before's queue), 30 s at most.
fn settle(session: &Arc<Session>) {
    let until = Instant::now() + Duration::from_secs(30);
    while session.queued() > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(1500));
}

fn main() {
    now_ns();
    let args: Vec<String> = std::env::args().collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let identity = Identity::from_pem_files(&value("--cert").expect("--cert"), &value("--key").expect("--key")).unwrap();
    let trusted = value("--trust").expect("--trust");
    let tablet = value("--tablet").expect("--tablet").parse().expect("an address");
    let seconds: f64 = value("--seconds").and_then(|s| s.parse().ok()).unwrap_or(6.0);
    let video_kbps: Option<u64> = value("--video").and_then(|v| v.parse().ok());
    let quic = args.iter().any(|a| a == "--quic");
    let opened: usize = value("--opens").and_then(|n| n.parse().ok()).unwrap_or(20);
    let node = Node::new("Bench PC", &identity, Arc::new(move |fp: &str| fp == trusted)).unwrap();
    node.upgrade_to_quic(quic);
    let session = node.connect(tablet, Duration::from_secs(5)).expect("a session");
    let started = Instant::now();
    while quic && !session.by_quic() && started.elapsed() < Duration::from_secs(6) {
        std::thread::sleep(Duration::from_millis(20));
    }
    while session.rtt().is_none() && started.elapsed() < Duration::from_secs(8) {
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("test=path transport={} path={:?}", if session.by_quic() { "quic" } else { "tcp" }, session.path());
    println!("test=rt_idle {}", realtime(&session, seconds));
    settle(&session);
    println!("test=up mbs={:.2}", transfer(&session, "sink", seconds));
    settle(&session);
    println!("test=both mbs={:.2}", transfer(&session, "echo", seconds));
    settle(&session);
    let both = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "echo", seconds))
    };
    let rt = realtime(&session, seconds);
    println!("test=rt_both {rt} mbs={:.2}", both.join().unwrap());
    settle(&session);
    println!("test=video {}", video(&session, seconds, video_kbps));
    settle(&session);
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", seconds))
    };
    let v = video(&session, seconds, video_kbps);
    println!("test=video_up {v} mbs={:.2}", up.join().unwrap());
    settle(&session);
    // The same 4 Mbit/s on both transports: the path alone compared.
    println!("test=video4 {}", video(&session, seconds, Some(4000)));
    settle(&session);
    let both = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "echo", seconds))
    };
    let (rtts, sent, gap) = flow(&session, Class::Input, 20, Duration::from_micros(3500), seconds);
    println!("test=input_both back={} sent={sent} gap={gap} {} mbs={:.2}", rtts.len(), quantiles(rtts), both.join().unwrap());
    settle(&session);
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", seconds))
    };
    let (rtts, sent, gap) = flow(&session, Class::Realtime, 200, Duration::from_millis(5), seconds);
    println!("test=audio_up back={} sent={sent} gap={gap} {} mbs={:.2}", rtts.len(), quantiles(rtts), up.join().unwrap());
    settle(&session);
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", 2.0 * seconds))
    };
    std::thread::sleep(Duration::from_millis(500));
    let (times, failed) = opens(&session, opened);
    println!("test=opens_up ok={} failed={failed} {} mbs={:.2}", times.len(), quantiles(times), up.join().unwrap());
    println!("test=end dropped={} rate_kbps={:?}", session.dropped(), session.rate().map(|r| r * 8 / 1000));
}
