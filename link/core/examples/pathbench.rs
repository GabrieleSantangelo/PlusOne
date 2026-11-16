//! The link's numbers on one path, from the PC's side, against
//! examples/tablet (tools/netbench.sh runs both on an emulated network):
//! realtime round trips idle and under a transfer both ways, a transfer one
//! way and both ways, and a video's frames (60 a second, a keyframe of four
//! every two seconds, in MAX_DATAGRAM pieces): how many came whole, and how
//! soon each was acknowledged; at a fixed bitrate, or following the
//! session's rate (a QUIC path's controller), and at a fixed 4 Mbit/s on
//! both transports; input (285 states a second) and sound (a packet each
//! 5 ms: its gaps) under a transfer, and beside the video; channels opened
//! while a transfer runs (how long, how many never were). The video is a
//! Video channel's, a frame a group (on QUIC, a stream each, if the tablet
//! reads them): after the sound beside it, what its groups did (av_groups),
//! and at the end in all (groups). One line a test, key=value.
//! With --chain S, only the desktop's chain after a cut, on QUIC (chain()):
//! S seconds as appdisplay.py ran before l36 (chain_old), S as after
//! (chain_new), on the same session; with --variant NAME[:KEY=VALUE...]
//! (once each), those instead (chain_NAME), in an order turned at random
//! (test=order): KEY l36 (0: before it), hold (ms), start (1: the first
//! bitrate the budget's share), pay (frames), depth (frames), fits, as
//! appdisplay.py's PLUSONE_L36* (Variant), each as it ships unless given;
//! fit (0: keyframes reckoned at VBV_FRAMES and none backed off, as l36
//! was first made); and the picture's: key (the encoder's keyframes so
//! many times VBV_FRAMES), ramp (s: the bitrate's target rising from
//! RAMP_FROM of the budget's share to all of it, a picture starting to
//! move: changes up, each a keyframe), low (kbit/s: under it the
//! encoder's keyframes heavier, by the square root of low over the
//! bitrate, as encoders overshoot), weigh (0: keyframes reckoned from all of the last
//! three, at any bitrate and age); and the link's: room (what the desktop
//! takes the link to hold of realtime, so many times what it does: past 1,
//! keyframes it took to fit cut all the same).
//! With --video-only, only the video's tests (video, video_up, av), its
//! keyframes answering the tablet's asks as the desktop does.
//!   pathbench --cert PEM --key PEM --trust FINGERPRINT --tablet ADDRESS
//!             [--quic] [--seconds S] [--video KBPS] [--opens N] [--chain S
//!             [--variant NAME[:KEY=VALUE...]]...] [--video-only]

use plusone_link::groups::Stats as GroupStats;
use plusone_link::session::MAX_DATAGRAM;
use plusone_link::{Class, Identity, Node, Session};
use std::collections::{HashMap, VecDeque};
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
/// less leave (bytes a second: the others' share, as the desktop leaves it):
/// a keyframe of four frames every two seconds, and one answering the
/// tablet's ask as the desktop does (a gap apart: examples/tablet asks as
/// DisplaySession.kt does, after a frame lost, a frame whole after a newer
/// one among them). Whole: the frames the tablet put together in order.
fn video(session: &Arc<Session>, seconds: f64, kbps: Option<u64>, leave: u64) -> String {
    let channel = session.open("frames", "v", Class::Video).unwrap();
    let reader = channel.try_clone().unwrap();
    let sent_at: Arc<Mutex<HashMap<u32, u64>>> = Arc::default();
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::default();
    let wholes: Arc<Mutex<std::collections::HashSet<u32>>> = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let asks = Arc::new(AtomicU64::new(0));
    let collecting = {
        let (sent_at, latencies, wholes, done, asks) =
            (sent_at.clone(), latencies.clone(), wholes.clone(), done.clone(), asks.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64];
            while !done.load(Ordering::SeqCst) {
                match reader.recv(&mut buf, Some(Duration::from_millis(100))) {
                    // The frame whole, then those whole before it (said
                    // again: their own answer may have been lost), timed
                    // only if this is the first heard of them.
                    Ok(Some(n)) if n >= 4 && n % 4 == 0 => {
                        for (i, piece) in buf[..n].chunks_exact(4).enumerate() {
                            let frame = u32::from_be_bytes(piece.try_into().unwrap());
                            if let Some(at) = sent_at.lock().unwrap().remove(&frame) {
                                wholes.lock().unwrap().insert(frame);
                                if i == 0 {
                                    latencies.lock().unwrap().push((now_ns() - at) as f64 / 1e6);
                                }
                            }
                        }
                    }
                    Ok(Some(1)) => {
                        asks.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        })
    };
    let payload = MAX_DATAGRAM - 8;
    let (start, mut frame, mut bytes, mut rates) = (Instant::now(), 0u32, 0u64, Vec::new());
    let (mut keyframes, mut key_next, mut last_keyframe) = (std::collections::HashSet::new(), false, f64::NEG_INFINITY);
    let (mut asks_seen, mut answered) = (0u64, 0u64);
    let mut skipped = std::collections::HashSet::new();
    while start.elapsed().as_secs_f64() < seconds {
        let due = start + Duration::from_secs_f64(frame as f64 / 60.0);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let now = start.elapsed().as_secs_f64();
        let gap = session.rtt().map_or(MIN_KEYFRAME_GAP, |rtt| (1.5 * rtt.as_secs_f64() + VBV_FRAMES / FPS).max(MIN_KEYFRAME_GAP));
        for _ in asks_seen..asks.load(Ordering::Relaxed) {
            asks_seen += 1;
            if now - last_keyframe >= gap {
                (key_next, answered) = (true, answered + 1);
            }
        }
        let rate = match kbps {
            Some(kbps) => kbps * 1000 / 8,
            None => session.rate().map(|r| (r * 85 / 100).saturating_sub(leave)).unwrap_or(250_000)
                .clamp(12_500, 2_500_000),
        };
        rates.push(rate);
        // As the desktop does (appdisplay.py QUEUE_MS): what waits to go out
        // past 60 ms of the rate (64 KB at least) and the frame is not
        // encoded; a keyframe waits for room for itself.
        let behind = session.queued() as u64;
        let room = (rate * 60 / 1000).max(64 << 10);
        let keyframe = frame % 120 == 0 || key_next;
        if behind > room || (keyframe && behind + (rate / 60) as u64 * 4 > 2 * room) {
            skipped.insert(frame);
            key_next |= keyframe;
            frame += 1;
            continue;
        }
        key_next = false;
        if keyframe {
            keyframes.insert(frame);
            last_keyframe = now;
        }
        let average = (rate / 60) as usize;
        let size = if keyframe { average * 4 } else { average }.max(1);
        let count = size.div_ceil(payload);
        sent_at.lock().unwrap().insert(frame, now_ns());
        for index in 0..count {
            let mut packet = frame.to_be_bytes().to_vec();
            packet.extend_from_slice(&(index as u16).to_be_bytes());
            packet.extend_from_slice(&(count as u16 | if keyframe { KEYFRAME_FLAG } else { 0 }).to_be_bytes());
            packet.resize(8 + payload.min(size - index * payload), 0);
            bytes += packet.len() as u64;
            let _ = channel.send_in_group(frame, keyframe, index + 1 == count, &packet);
        }
        frame += 1;
    }
    std::thread::sleep(Duration::from_millis(800));
    done.store(true, Ordering::SeqCst);
    let _ = collecting.join();
    let whole = wholes.lock().unwrap().len();
    let mean = rates.iter().sum::<u64>() / rates.len().max(1) as u64;
    let sent = frame as usize - skipped.len();
    // Whole frames after a lost one since their keyframe: a decoder could
    // not show them.
    let (wholes, mut broken, mut undecodable) = (wholes.lock().unwrap(), false, 0);
    for f in 0..frame {
        if skipped.contains(&f) {
            continue;
        }
        broken &= !keyframes.contains(&f);
        match wholes.contains(&f) {
            false => broken = true,
            true if broken => undecodable += 1,
            true => {}
        }
    }
    format!("whole={:.1}% decodable={:.1}% lost={} undecodable={undecodable} asks={asks_seen} answered={answered} \
             skipped={} keyframes={} kbps={} target={} {}",
            100.0 * whole as f64 / sent.max(1) as f64, 100.0 * (whole - undecodable) as f64 / sent.max(1) as f64,
            sent - whole, skipped.len(), keyframes.len(), (bytes as f64 * 8.0 / 1000.0 / seconds) as u64, mean * 8 / 1000,
            quantiles(latencies.lock().unwrap().clone()))
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
const HOLD: f64 = 0.0;
const QUEUE_MS: f64 = 60.0;
const MIN_KEYFRAME_GAP: f64 = 0.25;
const KEY_FLYING_MOST: f64 = 0.5;
const LATE: f64 = 0.1;
const LINK_ROOM: f64 = (512 << 10) as f64;
const TOLD: f64 = 0.9;
const ALONE: f64 = (8 << 10) as f64;
const PACKET_EXTRA: f64 = 80.0;
const ALLOWANCE_FRAMES: f64 = VBV_FRAMES + 1.0;
/// appdisplay.py's KEY_SIZES and KEY_BACKOFF (its KEY_WORTH: VBV_FRAMES
/// with the packets' extra, by the payload).
const KEY_SIZES: usize = 3;
const KEY_BACKOFF: f64 = 0.5;
/// appdisplay.py's KEY_AGE (two of media.py's periodic keyframes) and KEY_NEAR.
const KEY_AGE: f64 = 20.0;
const KEY_NEAR: f64 = 0.5;
/// A ramp's start: the bitrate's target this much of the budget's share
/// (live, 8.5 to 20 Mbit/s in 14 s as a video started in a browser, in
/// four changes).
const RAMP_FROM: f64 = 0.4;
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

/// One way the desktop runs in chain(), as appdisplay.py's PLUSONE_L36*
/// set it: before l36 (l36 false: the bitrate down at once), or after, the
/// bitrate down once the budget has been under it for hold (s), or sooner
/// once the frames left out for it are worth pay frames at its bitrate (0:
/// never); start, the first bitrate the budget's share (the first frame is
/// a keyframe anyway); the allowance depth frames deep, filling at fits of
/// the budget (also what the bitrate must stay under); fit, keyframes
/// reckoned from the last ones made and backed off once the link cut one
/// (false: at VBV_FRAMES, as l36 was first made). And the picture: the
/// encoder's keyframes key times VBV_FRAMES, and under low kbit/s the
/// square root of low over the bitrate times that; the bitrate's target ramping up over ramp
/// seconds; weigh, keyframes made long ago or under KEY_NEAR of the
/// bitrate reckoned left out of its reckoning. And the link: what the
/// desktop takes it to hold of realtime, room times what it does.
#[derive(Clone, Debug)]
struct Variant {
    name: String,
    l36: bool,
    hold: f64,
    start: bool,
    pay: f64,
    depth: f64,
    fits: f64,
    fit: bool,
    key: f64,
    ramp: f64,
    room: f64,
    low: f64,
    weigh: bool,
}

impl Variant {
    fn new(name: &str, l36: bool) -> Variant {
        Variant { name: name.into(), l36, hold: HOLD, start: l36, pay: 0.0, depth: ALLOWANCE_FRAMES, fits: FITS,
                  fit: l36, key: 1.0, ramp: 0.0, room: 1.0, low: 0.0, weigh: true }
    }

    /// NAME[:KEY=VALUE...], as --variant has it.
    fn parse(spec: &str) -> Variant {
        let mut parts = spec.split(':');
        let mut variant = Variant::new(parts.next().unwrap(), true);
        for part in parts {
            let (key, value) = part.split_once('=').expect("--variant NAME[:KEY=VALUE...]");
            let value: f64 = value.parse().expect("a number");
            match key {
                "l36" => variant.l36 = value != 0.0,
                "hold" => variant.hold = value / 1000.0,
                "start" => variant.start = value != 0.0,
                "pay" => variant.pay = value,
                "depth" => variant.depth = value,
                "fits" => variant.fits = value,
                "fit" => variant.fit = value != 0.0,
                "key" => variant.key = value,
                "ramp" => variant.ramp = value,
                "room" => variant.room = value,
                "low" => variant.low = value,
                "weigh" => variant.weigh = value != 0.0,
                _ => panic!("--variant: no key {key}"),
            }
        }
        variant
    }
}

/// RateControl.budget(): the bitrate to change to, if it does; after l36,
/// only once the budget has been under it for the variant's hold, or the
/// bytes it is over by (what the allowance leaves out) are worth pay frames.
/// Its target: scale of the budget's share (a ramp's).
struct Rate {
    kbps: f64,
    changed_at: f64,
    under: Option<f64>,
    over: f64,
    at: f64,
}

impl Rate {
    fn budget(&mut self, budget: f64, scale: f64, now: f64, v: &Variant) -> Option<f64> {
        let target = (budget * SHARE * scale).min(MOST_KBPS).max(MIN_KBPS).floor();
        let fits = self.kbps <= budget * v.fits;
        if fits {
            (self.under, self.over) = (None, 0.0);
        } else if self.under.is_none() {
            (self.under, self.over) = (Some(now), 0.0);
        } else {
            self.over += (self.kbps - budget * v.fits) * 125.0 * (now - self.at);
        }
        self.at = now;
        if target < self.kbps {
            let paid = v.pay > 0.0 && self.over >= v.pay * self.kbps * 125.0 / FPS;
            if fits || (v.l36 && now - self.under.unwrap_or(now) < v.hold && !paid) {
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

    /// RateControl.lower(): down to kbps for a keyframe that would not get
    /// through; up again UP_AFTER on at the earliest. The bitrate now.
    fn lower(&mut self, kbps: f64, now: f64) -> f64 {
        let kbps = kbps.min(MOST_KBPS).max(MIN_KBPS).floor();
        if kbps != self.kbps {
            (self.kbps, self.changed_at, self.under) = (kbps, now, None);
        }
        self.kbps
    }
}

/// appdisplay.py's _key_worth: a keyframe at kbps as reckoned, in average
/// frames at it, from the last made (worth, kbit/s, when): the most of
/// those made within KEY_AGE at KEY_NEAR of kbps or more (weigh false: of
/// all), floor at least.
fn key_worth_at(worths: &VecDeque<(f64, f64, f64)>, kbps: f64, now: f64, weigh: bool, floor: f64) -> f64 {
    worths.iter().filter(|&&(_, made, at)| !weigh || made >= kbps * KEY_NEAR && now - at <= KEY_AGE)
        .fold(floor, |most, &(worth, _, _)| most.max(worth))
}

/// appdisplay.py's _key_kbps: the most bitrate up to kbps whose keyframe,
/// as reckoned at it, takes no more than room (bytes * FPS / 125): each
/// stretch between the keyframes' bitrates over KEY_NEAR, at its top.
fn key_kbps(worths: &VecDeque<(f64, f64, f64)>, room: f64, kbps: f64, now: f64, weigh: bool, floor: f64) -> f64 {
    let mut edges: Vec<f64> = worths.iter().map(|&(_, made, at)| (made / KEY_NEAR, at))
        .filter(|&(edge, at)| weigh && now - at <= KEY_AGE && edge < kbps).map(|(edge, _)| edge).collect();
    edges.sort_by(f64::total_cmp);
    edges.dedup();
    edges.push(kbps);
    let (mut best, mut low) = (0.0f64, 0.0f64);
    for high in edges {
        let top = high.min(room / key_worth_at(worths, high, now, weigh, floor));
        if top > low {
            best = top;
        }
        low = high;
    }
    best.floor()
}

/// The desktop after a cut, on a picture that always moves (a frame offered
/// each 1/60 s, its size the bitrate's average, a keyframe VBV_FRAMES of
/// them), as appdisplay.py runs it: v.l36 false, before l36 (the bitrate
/// down at once when it no longer fits; each change a keyframe, as on
/// Intel's encoders; the tablet's asks answered a gap apart; frames left
/// out past QUEUE_MS of the budget waiting, 64 KB at least; every frame
/// sent, the link dropping what is late packet by packet); true, after
/// (the bitrate down after v.hold under the budget, or once the frames
/// left out are worth v.pay, frames left out within it meanwhile; no
/// keyframe while the last still waits to go out; a keyframe made once all
/// of it fits in what the link holds; a frame that would not fit whole not
/// sent, nor those after it until a keyframe; v.start, the first bitrate
/// the budget's share). v.fit, as appdisplay.py has it since: a keyframe
/// reckoned at the most of the last KEY_SIZES made (in average frames at
/// their bitrate, VBV_FRAMES' worth at least; v.weigh, only those made
/// within KEY_AGE at KEY_NEAR of the bitrate or more), made once it fits behind
/// what waits, and sent only if it still does as it goes (else not, nor
/// those after it); a bitrate whose keyframe could not fit with nothing
/// waiting down first (to the one whose does, the change its keyframe; a
/// change up only so far); an ask within the tablet's gap, the keyframe's
/// time out and two round trips of one going, the link having dropped
/// realtime since (weighed even when the gap leaves the ask unanswered,
/// once): the next at KEY_BACKOFF of the bitrate, the frames after
/// it left out until it is out (alone); no answer within the last
/// keyframe's time out and a round trip and a half. v.key: the encoder's
/// keyframes so many times VBV_FRAMES (it makes them as it likes: live,
/// two or three times; v.low: under it, the square root of low over the
/// bitrate times that, as encoders overshoot: still lighter at a lower
/// bitrate, as theirs are); v.ramp: the bitrate's target rising from RAMP_FROM
/// of the budget's share to all of it over so many seconds (a picture
/// starting to move; the budget told as it is); v.room: what the desktop
/// takes the link to hold, so many times what it does (past 1, the link
/// cuts what it took to fit: the back-off's case).
/// examples/tablet asks for a keyframe as DisplaySession.kt does (a frame
/// whole while an older one came in part; then at each frame that is not
/// one, 250 ms apart), and answers each frame whole. What the tablet shows:
/// a frame whole, and every one since the last keyframe whole too; the
/// keyframes that came whole (keys_whole), those backed off (cuts); and
/// how long the bitrate was over the budget (under), the frames left out
/// meanwhile for it (inhold).
fn chain(session: &Arc<Session>, seconds: f64, v: &Variant) -> String {
    let fix = v.l36;
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
                    Ok(Some(n)) if n >= 4 && n % 4 == 0 => {
                        for piece in buf[..n].chunks_exact(4) {
                            whole.entry(u32::from_be_bytes(piece.try_into().unwrap())).or_insert_with(now_ns);
                        }
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
    let ramp = |now: f64| if now < v.ramp { RAMP_FROM + (1.0 - RAMP_FROM) * now / v.ramp } else { 1.0 };
    let rate_kbps = || session.rate().map(|r| r as f64 * 8.0 / 1000.0).unwrap_or(0.0);
    let mut told = Told { kbps: rate_kbps(), at: 0.0 };
    let mut rate = Rate { kbps: MOST_KBPS, changed_at: f64::NEG_INFINITY, under: None, over: 0.0, at: 0.0 };
    if v.start && told.kbps > 0.0 {
        // Before the first frame, a keyframe anyway: no change to hold back.
        (rate.kbps, rate.changed_at) = ((told.kbps * SHARE * ramp(0.0)).min(MOST_KBPS).max(MIN_KBPS).floor(), 0.0);
    }
    let mut encoder = rate.kbps;
    let (mut under_for, mut under_left_out) = (0.0f64, 0u64);
    let (mut pending, mut key_next, mut since_key) = (None::<f64>, true, 0u64);
    let (mut wanted, mut broken, mut key_made, mut last_keyframe) = (false, false, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let (mut key_at, mut after_key, mut flying) = (None::<f64>, 0.0f64, false);
    let (mut level, mut level_at) = (f64::NAN, 0.0f64);
    let (mut asks_seen, mut answered, mut held, mut changes) = (0u64, 0u64, 0u64, 0u64);
    let (mut skipped, mut unsent, mut keyframes) = ([0u64; 4], [0u64; 2], 0u64);
    // v.fit: the last keyframes made (average frames' worth, kbit/s, when),
    // the last sent (bytes on the wire), the link's drops as it went, since
    // when the next goes alone, the keyframes backed off.
    let key_worth = VBV_FRAMES * (1.0 + PACKET_EXTRA / payload as f64);
    let (mut worths, mut key_size, mut key_dropped) = (VecDeque::<(f64, f64, f64)>::new(), 0.0f64, None::<u64>);
    let (mut alone_from, mut cuts) = (None::<f64>, 0u64);
    let mut sent: Vec<(u32, bool, f64, u64)> = Vec::new();  // number, keyframe, when, ns
    let (mut number, mut frame, mut bytes) = (0u32, 0u64, 0f64);
    while at(Instant::now()) < seconds {
        let due = start + Duration::from_secs_f64(frame as f64 / FPS);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        frame += 1;
        let now = at(Instant::now());
        let budget = told.update(rate_kbps(), now);
        let key_time = if key_size > 0.0 {
            key_size / (if budget > 0.0 { budget } else { encoder } * 125.0)
        } else {
            VBV_FRAMES / FPS
        };
        if rate.under.is_some() {
            under_for += 1.0 / FPS;
        }
        if budget > 0.0 {
            if let Some(kbps) = rate.budget(budget, ramp(now), now, v) {
                if fix {
                    pending = Some(kbps);
                } else {
                    (encoder, key_next, changes) = (kbps, true, changes + 1);
                }
            }
        }
        // The tablet's asks since the last frame.
        let rtt = session.rtt().map(|rtt| rtt.as_secs_f64());
        let gap = if fix && v.fit {
            (1.5 * rtt.unwrap_or(0.0) + key_time.max(VBV_FRAMES / FPS)).max(MIN_KEYFRAME_GAP)
        } else {
            rtt.map_or(MIN_KEYFRAME_GAP, |rtt| (1.5 * rtt + VBV_FRAMES / FPS).max(MIN_KEYFRAME_GAP))
        };
        for _ in asks_seen..asks.load(Ordering::Relaxed) {
            asks_seen += 1;
            if fix && flying {
                held += 1;
                continue;
            }
            // The last keyframe cut by the link, asked for again soon after:
            // weighed even within the gap (the ask comes 250 ms after the
            // one the keyframe answered), backed off once.
            let cut = fix && v.fit && key_dropped.is_some_and(|d| session.dropped() > d)
                && key_at.is_some_and(|k| now <= k + MIN_KEYFRAME_GAP + key_time + 2.0 * rtt.unwrap_or(0.0));
            if cut {
                (cuts, key_dropped, alone_from) = (cuts + 1, None, Some(now));
                let kbps = rate.lower((encoder * KEY_BACKOFF).floor().min(rate.kbps), now);
                pending = (kbps != encoder).then_some(kbps);
            }
            if now - last_keyframe >= gap {
                (last_keyframe, answered) = (now, answered + 1);
                if fix { wanted = true } else { key_next = true }
            }
        }
        // Into the encoder, or not.
        let queued = session.queued() as f64;
        let holds = (budget * 125.0 * LATE).min(LINK_ROOM) * TOLD * v.room;
        let fits = |size: f64, queued: f64| budget <= 0.0 || queued + size <= holds || queued <= ALONE;
        if fix {
            flying = key_at.is_some_and(|k| now - k < KEY_FLYING_MOST && queued > after_key);
            if let (Some(from), Some(k)) = (alone_from, key_at) {
                if k >= from {
                    if flying {
                        skipped[3] += 1;
                        continue;
                    }
                    alone_from = None;
                }
            }
            let most = (64.0 * 1024.0f64).max(rate.kbps.min(budget) * QUEUE_MS / 8.0);
            let most = if budget > 0.0 { most.min(holds - encoder * 125.0 / FPS).max(ALONE) } else { most };
            if queued > most {
                skipped[0] += 1;
                continue;
            }
            let allowed = budget * v.fits * 125.0;
            if budget > 0.0 {
                let depth = allowed * v.depth / FPS;
                level = if level.is_nan() { depth } else { (level + allowed * (now - level_at)).min(depth) };
                level_at = now;
                if level < 0.0 {
                    skipped[1] += 1;
                    under_left_out += rate.under.is_some() as u64;
                    continue;
                }
            }
            let key_due = wanted || (broken && now - key_made >= MIN_KEYFRAME_GAP);
            if v.fit && budget > 0.0 && (key_due || pending.is_some()) {
                // Its keyframe never past what the link holds: down first.
                let kbps = pending.unwrap_or(encoder);
                let room = (holds - ALONE) * FPS / 125.0;
                let most = key_kbps(&worths, room, kbps, now, v.weigh, key_worth).max(MIN_KBPS);
                if kbps > most {
                    let kbps = rate.lower(most.min(rate.kbps), now);
                    pending = (kbps != encoder).then_some(kbps);
                }
            }
            if key_due || pending.is_some() {
                let kbps = pending.unwrap_or(encoder);
                let worth = if v.fit { key_worth_at(&worths, kbps, now, v.weigh, key_worth) } else { key_worth };
                let size = kbps * 125.0 / FPS * worth;
                let room = if v.fit {
                    budget <= 0.0 || queued + size <= holds || kbps <= MIN_KBPS && queued <= ALONE
                } else {
                    fits(size, queued)
                };
                if flying || !room {
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
        let heavier = if v.low > 0.0 { (v.low / encoder).max(1.0).sqrt() } else { 1.0 };
        let frames = if keyframe { VBV_FRAMES * v.key * heavier } else { 1.0 };
        let size = (encoder * 125.0 / FPS * frames).max(1.0) as usize;
        let count = size.div_ceil(payload);
        let wire = size as f64 + count as f64 * PACKET_EXTRA;
        if fix {
            if broken && !keyframe {
                unsent[1] += 1;
                continue;
            }
            if keyframe && v.fit {
                if worths.len() == KEY_SIZES {
                    worths.pop_front();
                }
                worths.push_back((wire * FPS / (encoder * 125.0), encoder, now));
            }
            let queued = session.queued() as f64;
            let room = if keyframe && v.fit {
                budget <= 0.0 || queued + wire <= holds || encoder <= MIN_KBPS && queued <= ALONE
            } else {
                fits(wire, queued)
            };
            if !room {
                unsent[0] += 1;
                (broken, wanted) = (true, true);
                continue;
            }
            if keyframe && v.fit {
                key_dropped = Some(session.dropped());
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
                key_size = wire;
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
    let last_second = shown.len() - 1;
    let mut lost = 0;
    for &(n, keyframe, when, ns) in &sent {
        match whole.get(&n) {
            Some(&back) => {
                decodable |= keyframe;
                if decodable {
                    shown[(when as usize).min(last_second)] += 1;
                    latencies.push((back - ns) as f64 / 1e6);
                }
            }
            None => (lost, decodable) = (lost + 1, false),
        }
    }
    let shown_frames: u32 = shown.iter().sum();
    let frozen = shown[..seconds.floor() as usize].iter().filter(|&&n| n == 0).count();
    let keys_whole = sent.iter().filter(|&&(n, keyframe, _, _)| keyframe && whole.contains_key(&n)).count();
    format!("shown={:.1}% frozen={frozen} lost={lost} keyframes={keyframes} keys_whole={keys_whole} asks={asks_seen} \
             answered={answered} held={held} cuts={cuts} changes={changes} skipped={} queue={} budget={} forkey={} \
             alone={} unsent={} late={} kbps={} {} under={under_for:.1} inhold={under_left_out}",
            100.0 * shown_frames as f64 / frame.max(1) as f64, skipped.iter().sum::<u64>(), skipped[0], skipped[1],
            skipped[2], skipped[3], unsent.iter().sum::<u64>(), session.dropped() - dropped,
            (bytes * 8.0 / 1000.0 / seconds) as u64, quantiles(latencies))
}

/// What a QUIC path's groups did between before and after (all since it
/// began, if before is None), and how much of what was lost soon after a
/// cut QUIC sent again, estimated: the groups' share of the realtime sent
/// (bulk aside).
fn groups(before: Option<GroupStats>, after: Option<GroupStats>) -> String {
    let Some(after) = after else { return "none=1".into() };
    let b = before.unwrap_or_default();
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let (stream, datagram) = (d(after.stream_bytes, b.stream_bytes), d(after.datagram_bytes, b.datagram_bytes));
    let share = stream as f64 / (stream + datagram).max(1) as f64;
    let lost_cut = d(after.lost_after_cut, b.lost_after_cut);
    format!("groups={} late={} late_kb={:.1} superseded={} late_unsent={} stopped={} resets={} dropped={} \
             credit_waits={} window_full={} wait_ms={} control={} stream_kb={:.1} lost_kb={:.1} lost_cut_kb={:.1} cuts={} \
             share={:.2} retx_cut_kb={:.1}",
            d(after.groups, b.groups), d(after.late, b.late), d(after.late_bytes, b.late_bytes) as f64 / 1e3,
            d(after.superseded, b.superseded), d(after.late_unsent, b.late_unsent), d(after.stopped, b.stopped),
            d(after.resets, b.resets), d(after.dropped, b.dropped),
            d(after.credit_waits, b.credit_waits), d(after.window_full, b.window_full), d(after.wait_ms, b.wait_ms),
            d(after.control, b.control), stream as f64 / 1e3, d(after.lost_bytes, b.lost_bytes) as f64 / 1e3,
            lost_cut as f64 / 1e3, d(after.cuts, b.cuts), share, lost_cut as f64 * share / 1e3)
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
    let mut variants: Vec<Variant> = args.windows(2).filter(|w| w[0] == "--variant").map(|w| Variant::parse(&w[1])).collect();
    if variants.is_empty() {
        variants = vec![Variant::new("old", false), Variant::new("new", true)];
    } else {
        // Each variant as often first as last, over the runs.
        let turn = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()
            ^ std::process::id().wrapping_mul(2_654_435_761)) as usize % variants.len();
        variants.rotate_left(turn);
    }
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
    if args.iter().any(|a| a == "--video-only") {
        // The video alone, beside an upload, and beside the sound.
        let before = session.group_stats();
        println!("test=video {}", video(&session, seconds, video_kbps, 0));
        if session.by_quic() {
            println!("test=video_groups {}", groups(before, session.group_stats()));
        }
        settle(&session);
        let before = session.group_stats();
        let up = {
            let session = session.clone();
            std::thread::spawn(move || transfer(&session, "sink", seconds))
        };
        let v = video(&session, seconds, video_kbps, 0);
        println!("test=video_up {v} mbs={:.2}", up.join().unwrap());
        if session.by_quic() {
            println!("test=up_groups {}", groups(before, session.group_stats()));
        }
        settle(&session);
        let before = session.group_stats();
        let sound = {
            let session = session.clone();
            std::thread::spawn(move || flow(&session, Class::Realtime, 960, Duration::from_millis(5), seconds))
        };
        let v = video(&session, seconds, video_kbps, SOUND);
        let (rtts, sent, gap) = sound.join().unwrap();
        println!("test=av_audio back={} sent={sent} gap={gap} {}", rtts.len(), quantiles(rtts));
        println!("test=av_video {v}");
        if session.by_quic() {
            println!("test=av_groups {}", groups(before, session.group_stats()));
        }
        end(&session);
        return;
    }
    if let Some(chained) = chained {
        // The budget is a QUIC path's controller's: on TCP, nothing to follow.
        if session.by_quic() {
            println!("test=order chain={}", variants.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(","));
            for variant in &variants {
                settle(&session);
                println!("test=chain_{} {}", variant.name, chain(&session, chained, variant));
            }
        }
        end(&session);
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
    let before = session.group_stats();
    println!("test=video {}", video(&session, seconds, video_kbps, 0));
    if session.by_quic() {
        println!("test=video_groups {}", groups(before, session.group_stats()));
    }
    settle(&session);
    let before = session.group_stats();
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", seconds))
    };
    let v = video(&session, seconds, video_kbps, 0);
    println!("test=video_up {v} mbs={:.2}", up.join().unwrap());
    if session.by_quic() {
        println!("test=up_groups {}", groups(before, session.group_stats()));
    }
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
    let before = session.group_stats();
    let sound = {
        let session = session.clone();
        std::thread::spawn(move || flow(&session, Class::Realtime, 960, Duration::from_millis(5), seconds))
    };
    let v = video(&session, seconds, video_kbps, SOUND);
    let (rtts, sent, gap) = sound.join().unwrap();
    println!("test=av_audio back={} sent={sent} gap={gap} {}", rtts.len(), quantiles(rtts));
    println!("test=av_video {v}");
    if session.by_quic() {
        println!("test=av_groups {}", groups(before, session.group_stats()));
    }
    settle(&session);
    // Channels opened while the video goes alone: a control message's wait
    // behind its frames.
    let frames = {
        let session = session.clone();
        std::thread::spawn(move || video(&session, seconds, video_kbps, 0))
    };
    std::thread::sleep(Duration::from_millis(500));
    let (times, failed) = opens(&session, 20);
    let _ = frames.join();
    println!("test=opens_video ok={} failed={failed} {}", times.len(), quantiles(times));
    settle(&session);
    let up = {
        let session = session.clone();
        std::thread::spawn(move || transfer(&session, "sink", 2.0 * seconds))
    };
    std::thread::sleep(Duration::from_millis(500));
    let (times, failed) = opens(&session, opened);
    println!("test=opens_up ok={} failed={failed} {} mbs={:.2}", times.len(), quantiles(times), up.join().unwrap());
    if session.by_quic() {
        println!("test=groups {}", groups(None, session.group_stats()));
    }
    end(&session);
}

/// The bench's end said (test=end), and the session closed: the tablet
/// says what is left of its counts (its groups' last lines, Seconds) before
/// netbench.sh reads them.
fn end(session: &Arc<Session>) {
    println!("test=end dropped={} rate_kbps={:?}", session.dropped(), session.rate().map(|r| r * 8 / 1000));
    session.close();
}
