//! The link's numbers on one path, from the PC's side, against
//! examples/tablet (tools/netbench.sh runs both on an emulated network):
//! realtime round trips idle and under a transfer both ways, a transfer one
//! way and both ways, and a video's frames (60 a second, a keyframe of four
//! every two seconds, in MAX_DATAGRAM pieces): how many came whole, and how
//! soon each was acknowledged; at a fixed bitrate, or following the
//! session's rate (a QUIC path's controller), and at a fixed 4 Mbit/s on
//! both transports; input (285 states a second) and sound (a packet each
//! 5 ms: its gaps) under a transfer, and beside the video; channels opened
//! while a transfer runs (how long, how many never were). One line a test,
//! key=value.
//! With --chain S, only the desktop's chain after a cut, on QUIC (chain()):
//! S seconds as appdisplay.py ran before l36 (chain_old), S as after
//! (chain_new), on the same session.
//!   pathbench --cert PEM --key PEM --trust FINGERPRINT --tablet ADDRESS
//!             [--quic] [--seconds S] [--video KBPS] [--opens N] [--chain S]

use plusone_link::session::MAX_DATAGRAM;
use plusone_link::{Class, Identity, Node, Session};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// The sound beside the video: PCM's 960 bytes each 5 ms, and what they
/// take on the network (seal, number, QUIC, UDP, IP), bytes a second.
const SOUND: u64 = (960 + 94) * 200;

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

/// A video for seconds at kbps, or (None) at 0.85 of the session's rate
/// less leave (bytes a second: the others' share, as the desktop leaves it).
fn video(session: &Arc<Session>, seconds: f64, kbps: Option<u64>, leave: u64) -> String {
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
            None => session.rate().map(|r| (r * 85 / 100).saturating_sub(leave)).unwrap_or(250_000)
                .clamp(12_500, 2_500_000),
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

/// The desktop's numbers, as plusone/appdisplay.py has them (media.py's,
/// the preset of 20 Mbit/s at 1920x1200), for chain().
const FPS: f64 = 60.0;
const VBV_FRAMES: f64 = 4.0;
const GOP: u64 = 600;
const MOST_KBPS: f64 = 20_000.0;
const MIN_KBPS: f64 = 800.0;
const SHARE: f64 = 0.85;
const FITS: f64 = 0.95;
const UP: f64 = 1.15;
const STEADY: f64 = 0.05;
const UP_AFTER: f64 = 2.0;
const HOLD: f64 = 1.0;
const QUEUE_MS: f64 = 60.0;
const MIN_KEYFRAME_GAP: f64 = 0.25;
const KEY_FLYING_MOST: f64 = 0.5;
const LATE: f64 = 0.1;
const LINK_ROOM: f64 = (512 << 10) as f64;
const TOLD: f64 = 0.9;
const ALONE: f64 = (8 << 10) as f64;
const PACKET_EXTRA: f64 = 80.0;
const ALLOWANCE_FRAMES: f64 = VBV_FRAMES + 1.0;
/// A frame's packets' count says it is a keyframe (examples/tablet asks as
/// DisplaySession.kt does once it sees one).
const KEYFRAME_FLAG: u16 = 0x8000;

/// The budget as plus-one-link tells it (cc.rs PUBLISH_CHANGE): past 10 %
/// of the last told, down at once, up at most every 100 ms; kbit/s.
struct Told {
    kbps: f64,
    at: f64,
}

impl Told {
    fn update(&mut self, rate_kbps: f64, now: f64) -> f64 {
        if self.kbps <= 0.0
            || (rate_kbps - self.kbps).abs() > 0.1 * self.kbps && (rate_kbps < self.kbps || now - self.at >= 0.1) {
            (self.kbps, self.at) = (rate_kbps, now);
        }
        self.kbps
    }
}

/// RateControl.budget(): the bitrate to change to, if it does; fix: only
/// after the budget has been under it for HOLD.
struct Rate {
    kbps: f64,
    changed_at: f64,
    under: Option<f64>,
}

impl Rate {
    fn budget(&mut self, budget: f64, now: f64, fix: bool) -> Option<f64> {
        let target = (budget * SHARE).min(MOST_KBPS).max(MIN_KBPS).floor();
        let fits = self.kbps <= budget * FITS;
        if fits {
            self.under = None;
        } else if self.under.is_none() {
            self.under = Some(now);
        }
        if target < self.kbps {
            if fits || (fix && now - self.under.unwrap_or(now) < HOLD) {
                return None;
            }
        } else if target < self.kbps * UP && (target < MOST_KBPS || target < self.kbps * (1.0 + STEADY))
            || now - self.changed_at < UP_AFTER {
            return None;
        }
        self.under = None;
        if target == self.kbps {
            return None;
        }
        (self.kbps, self.changed_at) = (target, now);
        Some(target)
    }
}

/// The desktop after a cut, on a picture that always moves (a frame offered
/// each 1/60 s, its size the bitrate's average, a keyframe VBV_FRAMES of
/// them), as appdisplay.py runs it: fix false, before l36 (the bitrate
/// down at once when it no longer fits; each change a keyframe, as on
/// Intel's encoders; the tablet's asks answered a gap apart; frames left
/// out past QUEUE_MS of the budget waiting, 64 KB at least; every frame
/// sent, the link dropping what is late packet by packet); fix true, after
/// (the bitrate down after HOLD under the budget, frames left out within
/// it meanwhile; no keyframe while the last still waits to go out; a
/// keyframe made once all of it fits in what the link holds; a frame that
/// would not fit whole not sent, nor those after it until a keyframe).
/// examples/tablet asks for a keyframe as DisplaySession.kt does (a frame
/// whole while an older one came in part; then at each frame that is not
/// one, 250 ms apart), and answers each frame whole. What the tablet shows:
/// a frame whole, and every one since the last keyframe whole too.
fn chain(session: &Arc<Session>, seconds: f64, fix: bool) -> String {
    let channel = session.open("frames", "chain", Class::Realtime).unwrap();
    let reader = channel.try_clone().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let asks = Arc::new(AtomicU64::new(0));
    let collecting = {
        let (done, asks) = (done.clone(), asks.clone());
        std::thread::spawn(move || {
            let (mut buf, mut whole) = (vec![0u8; 64], HashMap::new());
            while !done.load(Ordering::SeqCst) {
                match reader.recv(&mut buf, Some(Duration::from_millis(100))) {
                    Ok(Some(4)) => {
                        whole.insert(u32::from_be_bytes(buf[..4].try_into().unwrap()), now_ns());
                    }
                    Ok(Some(1)) => {
                        asks.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            whole
        })
    };
    let payload = MAX_DATAGRAM - 8;
    let dropped = session.dropped();
    let start = Instant::now();
    let at = |instant: Instant| instant.duration_since(start).as_secs_f64();
    let rate_kbps = || session.rate().map(|r| r as f64 * 8.0 / 1000.0).unwrap_or(0.0);
    let mut told = Told { kbps: rate_kbps(), at: 0.0 };
    let mut rate = Rate { kbps: MOST_KBPS, changed_at: f64::NEG_INFINITY, under: None };
    let mut encoder = MOST_KBPS;
    let (mut pending, mut key_next, mut since_key) = (None::<f64>, true, 0u64);
    let (mut wanted, mut broken, mut key_made, mut last_keyframe) = (false, false, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let (mut key_at, mut after_key, mut flying) = (None::<f64>, 0.0f64, false);
    let (mut level, mut level_at) = (f64::NAN, 0.0f64);
    let (mut asks_seen, mut answered, mut held, mut changes) = (0u64, 0u64, 0u64, 0u64);
    let (mut skipped, mut unsent, mut keyframes) = ([0u64; 3], [0u64; 2], 0u64);
    let mut sent: Vec<(u32, bool, f64, u64)> = Vec::new();  // number, keyframe, when, ns
    let (mut number, mut frame, mut bytes) = (0u32, 0u64, 0f64);
    while at(Instant::now()) < seconds {
        let due = start + Duration::from_secs_f64(frame as f64 / FPS);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        frame += 1;
        let now = at(Instant::now());
        let budget = told.update(rate_kbps(), now);
        if budget > 0.0 {
            if let Some(kbps) = rate.budget(budget, now, fix) {
                if fix {
                    pending = Some(kbps);
                } else {
                    (encoder, key_next, changes) = (kbps, true, changes + 1);
                }
            }
        }
        // The tablet's asks since the last frame.
        let gap = session.rtt().map_or(MIN_KEYFRAME_GAP, |rtt| (1.5 * rtt.as_secs_f64() + VBV_FRAMES / FPS).max(MIN_KEYFRAME_GAP));
        for _ in asks_seen..asks.load(Ordering::Relaxed) {
            asks_seen += 1;
            if fix && flying {
                held += 1;
            } else if now - last_keyframe >= gap {
                (last_keyframe, answered) = (now, answered + 1);
                if fix { wanted = true } else { key_next = true }
            }
        }
        // Into the encoder, or not.
        let queued = session.queued() as f64;
        let holds = (budget * 125.0 * LATE).min(LINK_ROOM) * TOLD;
        let fits = |size: f64, queued: f64| budget <= 0.0 || queued + size <= holds || queued <= ALONE;
        if fix {
            flying = key_at.is_some_and(|k| now - k < KEY_FLYING_MOST && queued > after_key);
            let most = (64.0 * 1024.0f64).max(rate.kbps.min(budget) * QUEUE_MS / 8.0);
            let most = if budget > 0.0 { most.min(holds - encoder * 125.0 / FPS).max(ALONE) } else { most };
            if queued > most {
                skipped[0] += 1;
                continue;
            }
            let allowed = budget * FITS * 125.0;
            if budget > 0.0 {
                let depth = allowed * ALLOWANCE_FRAMES / FPS;
                level = if level.is_nan() { depth } else { (level + allowed * (now - level_at)).min(depth) };
                level_at = now;
                if level < 0.0 {
                    skipped[1] += 1;
                    continue;
                }
            }
            let key_due = wanted || (broken && now - key_made >= MIN_KEYFRAME_GAP);
            if key_due || pending.is_some() {
                let kbps = pending.unwrap_or(encoder);
                let size = kbps * 125.0 * VBV_FRAMES / FPS * (1.0 + PACKET_EXTRA / payload as f64);
                if flying || !fits(size, queued) {
                    if key_due {
                        skipped[2] += 1;
                        continue;
                    }
                } else {
                    if let Some(kbps) = pending.take() {
                        (encoder, key_next, changes) = (kbps, true, changes + 1);
                    }
                    if key_due {
                        (wanted, key_made, key_next) = (false, now, true);
                    }
                }
            }
        } else if queued > (64.0 * 1024.0f64).max(rate.kbps.min(if budget > 0.0 { budget } else { rate.kbps }) * QUEUE_MS / 8.0) {
            skipped[0] += 1;
            continue;
        }
        // Encoded at once (measured: each frame out before the next goes in).
        let keyframe = key_next || since_key + 1 >= GOP;
        (key_next, since_key) = (false, if keyframe { 0 } else { since_key + 1 });
        let size = (encoder * 125.0 / FPS * if keyframe { VBV_FRAMES } else { 1.0 }).max(1.0) as usize;
        let count = size.div_ceil(payload);
        let wire = size as f64 + count as f64 * PACKET_EXTRA;
        if fix {
            if broken && !keyframe {
                unsent[1] += 1;
                continue;
            }
            if !fits(wire, session.queued() as f64) {
                unsent[0] += 1;
                (broken, wanted) = (true, true);
                continue;
            }
        }
        number += 1;
        keyframes += keyframe as u64;
        for index in 0..count {
            let mut packet = number.to_be_bytes().to_vec();
            packet.extend_from_slice(&(index as u16).to_be_bytes());
            packet.extend_from_slice(&(count as u16 | if keyframe { KEYFRAME_FLAG } else { 0 }).to_be_bytes());
            packet.resize(8 + payload.min(size - index * payload), 0);
            let _ = channel.send(&packet);
        }
        bytes += size as f64;
        sent.push((number, keyframe, now, now_ns()));
        if fix {
            level -= wire;
            if keyframe {
                (key_at, after_key, last_keyframe, broken, wanted) = (Some(now), 0.0, last_keyframe.max(now), false, false);
            } else {
                after_key += wire;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(800));
    done.store(true, Ordering::SeqCst);
    let whole = collecting.join().unwrap();
    // Shown: whole, with every frame since the last keyframe whole.
    let (mut decodable, mut shown, mut latencies) = (false, vec![0u32; seconds.ceil() as usize + 1], Vec::new());
    let mut lost = 0;
    for &(n, keyframe, when, ns) in &sent {
        match whole.get(&n) {
            Some(&back) => {
                decodable |= keyframe;
                if decodable {
                    shown[(when as usize).min(shown.len() - 1)] += 1;
                    latencies.push((back - ns) as f64 / 1e6);
                }
            }
            None => (lost, decodable) = (lost + 1, false),
        }
    }
    let shown_frames: u32 = shown.iter().sum();
    let frozen = shown[..seconds.floor() as usize].iter().filter(|&&n| n == 0).count();
    format!("shown={:.1}% frozen={frozen} lost={lost} keyframes={keyframes} asks={asks_seen} answered={answered} \
             held={held} changes={changes} skipped={} queue={} budget={} forkey={} unsent={} late={} kbps={} {}",
            100.0 * shown_frames as f64 / frame.max(1) as f64, skipped.iter().sum::<u64>(), skipped[0], skipped[1],
            skipped[2], unsent.iter().sum::<u64>(), session.dropped() - dropped,
            (bytes * 8.0 / 1000.0 / seconds) as u64, quantiles(latencies))
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
    let chained: Option<f64> = value("--chain").and_then(|s| s.parse().ok());
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
    if let Some(chained) = chained {
        // The budget is a QUIC path's controller's: on TCP, nothing to follow.
        if session.by_quic() {
            settle(&session);
            println!("test=chain_old {}", chain(&session, chained, false));
            settle(&session);
            println!("test=chain_new {}", chain(&session, chained, true));
        }
        println!("test=end dropped={} rate_kbps={:?}", session.dropped(), session.rate().map(|r| r * 8 / 1000));
        return;
    }
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
    println!("test=video {}", video(&session, seconds, video_kbps, 0));
    settle(&session);
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", seconds))
    };
    let v = video(&session, seconds, video_kbps, 0);
    println!("test=video_up {v} mbs={:.2}", up.join().unwrap());
    settle(&session);
    // The same 4 Mbit/s on both transports: the path alone compared.
    println!("test=video4 {}", video(&session, seconds, Some(4000), 0));
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
    // The desktop and its sound together, as the tablet takes them (PCM's
    // 960 bytes each 5 ms): the sound must not wait behind the frames.
    let sound = {
        let session = session.clone();
        std::thread::spawn(move || flow(&session, Class::Realtime, 960, Duration::from_millis(5), seconds))
    };
    let v = video(&session, seconds, video_kbps, SOUND);
    let (rtts, sent, gap) = sound.join().unwrap();
    println!("test=av_audio back={} sent={sent} gap={gap} {}", rtts.len(), quantiles(rtts));
    println!("test=av_video {v}");
    settle(&session);
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", 2.0 * seconds))
    };
    std::thread::sleep(Duration::from_millis(500));
    let (times, failed) = opens(&session, opened);
    println!("test=opens_up ok={} failed={failed} {} mbs={:.2}", times.len(), quantiles(times), up.join().unwrap());
    println!("test=end dropped={} rate_kbps={:?}", session.dropped(),
             session.rate().map(|r| r * 8 / 1000));
}
