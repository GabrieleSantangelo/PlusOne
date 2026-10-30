//! Paths taken down and back under a desktop's load (tools/switchbench.sh
//! runs it on emulated networks). The PC's side sends what the desktop and
//! the speakers do: a video (60 frames a second, keyframes of four frames
//! every 10 s and when the tablet asks, at --video kbit/s or 0.85 of the
//! path's budget less the sound's), its sound (a packet each 5 ms, PCM,
//! Opus while the budget is under 8 Mbit/s) and control messages (20 a
//! second); and it makes the paths as plus-one-link does: the first of
//! --path it can reach, the next of them in order at once when the
//! session has none and each second after, and, from a path of
//! --upgrade-from, the ones before it each 2 s. The tablet's side takes
//! them and records each frame (whole, decodable: a keyframe or the frame
//! after one decodable; one that is not asks for a keyframe, again each
//! 250 ms until one comes), sound packet and control message as it comes,
//! and sends the pen's input (285 a second), which the PC's records. Each
//! side writes its events (the node's, the dials, its threads and
//! descriptors each second) and what it sent (sent-*.csv) or received
//! (got-*.csv) in DIR, with CLOCK_MONOTONIC's ns, the same in each
//! container of a host: tools/switchbench-summary.py sums them up cycle by
//! cycle. A "bt" path is a stream (a UNIX socket, as an
//! RFCOMM socket handed over), its socket's room as small as RFCOMM's.
//!   switchbench tablet --cert PEM --key PEM --trust FINGERPRINT --out DIR
//!                      [--listen ADDRESS] [--bt SOCKET]
//!   switchbench pc --cert PEM --key PEM --trust FINGERPRINT --out DIR
//!                  --path KIND=ADDRESS|KIND=unix:SOCKET [--path ...]
//!                  [--quic] [--video KBPS] [--upgrade-from KIND[,KIND]]
//!   switchbench now        CLOCK_MONOTONIC in ns (the driver's times)
//! The PC's side writes the path it is on in DIR/current ("none" while
//! there is none).

use plusone_link::session::MAX_DATAGRAM;
use plusone_link::{Channel, Class, Event, Identity, Incoming, Node, Session};
use serde_json::json;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const FPS: u64 = 60;
/// A keyframe each this long, and on the tablet's asking no sooner than
/// max(KEY_GAP, 1.5 round trips + 4 frames) after the last (appdisplay.py).
const KEY_EVERY: Duration = Duration::from_secs(10);
const KEY_GAP: Duration = Duration::from_millis(250);
/// The tablet asks again for a keyframe this long after, until one comes.
const KEY_ASK_AGAIN: Duration = Duration::from_millis(250);
/// A video message's header: frame u32, index u16, count u16, keyframe u8,
/// sent (ns) u64.
const VIDEO_HEADER: usize = 17;
/// The others' header: number u32, sent (ns) u64, tag u8 (the sound's
/// codec: 0 PCM, 1 Opus).
const HEADER: usize = 13;
/// The sound's packets each 5 ms: PCM's 960 bytes, or Opus at 160 kbit/s.
const PCM: usize = 960;
const OPUS: usize = 100;
/// What each takes on the network a second (seal, number, UDP, IP).
const PCM_SHARE: u64 = (PCM + HEADER) as u64 * 200 + 94 * 200;
const OPUS_SHARE: u64 = (OPUS + HEADER) as u64 * 200 + 94 * 200;
/// pcm.py: Opus at once under OPUS_UNDER kbit/s of budget; PCM again once
/// over PCM_OVER (or none) for PCM_AFTER.
const OPUS_UNDER: u64 = 8000;
const PCM_OVER: u64 = 12000;
const PCM_AFTER: Duration = Duration::from_secs(10);
/// plus-one-link's dialing: a TCP connection waits this long (reach()), a
/// session with no path is reached for again each DIAL_EVERY, and from a
/// path of --upgrade-from the better ones are tried each UPGRADE_EVERY.
const DIAL_WAIT: Duration = Duration::from_millis(1500);
const DIAL_EVERY: Duration = Duration::from_secs(1);
const UPGRADE_EVERY: Duration = Duration::from_secs(2);
/// A Bluetooth-like stream's room, each way (the kernel doubles it): about
/// an RFCOMM socket's (40 credits of 127 bytes, ten times).
const STREAM_ROOM: libc::c_int = 24 << 10;

fn now_ns() -> u64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// A file of lines, flushed each second (resources()), events at once.
struct Lines(Mutex<BufWriter<File>>);

impl Lines {
    fn create(dir: &Path, name: &str) -> Arc<Lines> {
        let file = File::create(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        Arc::new(Lines(Mutex::new(BufWriter::new(file))))
    }

    fn line(&self, line: &str) {
        let _ = writeln!(self.0.lock().unwrap(), "{line}");
    }

    fn flush(&self) {
        let _ = self.0.lock().unwrap().flush();
    }
}

fn stream_room(stream: &UnixStream) {
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let size = STREAM_ROOM;
        unsafe {
            libc::setsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, option, &size as *const _ as *const libc::c_void,
                             std::mem::size_of::<libc::c_int>() as libc::socklen_t);
        }
    }
}

/// The node's events, each a line of events at once; on the PC's side the
/// path it is on in current too, and the dialer woken when it has none.
fn watch(node: &Node, events: Arc<Lines>, current: Option<PathBuf>, wake: Option<mpsc::Sender<()>>) {
    let wake = wake.map(Mutex::new);
    node.on_event(move |event| {
        let at = now_ns();
        let (line, on) = match &event {
            Event::Up(s) => (json!({"at": at, "ev": "up", "path": s.path(), "transport": s.transport()}), Some(s.path())),
            Event::Moved(s) => (json!({"at": at, "ev": "moved", "path": s.path(), "transport": s.transport()}), Some(s.path())),
            Event::Detached(_) => (json!({"at": at, "ev": "detached"}), Some(None)),
            Event::Budget(_, rate) => (json!({"at": at, "ev": "budget", "kbps": rate * 8 / 1000}), None),
            Event::Path(_, path) => (json!({"at": at, "ev": "path", "id": path.id, "kind": path.kind, "state": path.state.name(),
                                            "transport": path.transport, "silent_ms": path.silent_ms}), None),
            Event::Gone(_, why) => (json!({"at": at, "ev": "gone", "why": why}), Some(None)),
        };
        events.line(&line.to_string());
        events.flush();
        let Some(on) = on else { return };
        if let Some(current) = &current {
            let part = current.with_extension("part");
            if std::fs::write(&part, on.as_deref().unwrap_or("none")).is_ok() {
                let _ = std::fs::rename(&part, current);
            }
        }
        if on.is_none() {
            if let Some(wake) = &wake {
                let _ = wake.lock().unwrap().send(());
            }
        }
    });
}

/// Each second: this process's threads and descriptors in events, and
/// every file flushed.
fn resources(events: Arc<Lines>, files: Vec<Arc<Lines>>) {
    std::thread::spawn(move || loop {
        let count = |dir: &str| std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0);
        events.line(&json!({"at": now_ns(), "ev": "res", "threads": count("/proc/self/task"),
                            "fds": count("/proc/self/fd")}).to_string());
        events.flush();
        for file in &files {
            file.flush();
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

// -- the tablet ------------------------------------------------------------------

/// Each whole frame: frame, keyframe, sent, arrived, decodable.
fn receive_video(incoming: Incoming, lines: &Lines) {
    let channel = incoming.channel;
    let mut buf = vec![0u8; 1 << 16];
    let mut partial: HashMap<u32, (u16, u16)> = HashMap::new();
    let (mut last_ok, mut newest, mut asked): (Option<u32>, u32, Option<Instant>) = (None, 0, None);
    while let Ok(Some(n)) = channel.recv(&mut buf, None) {
        if n == 0 {
            break;
        }
        if n < VIDEO_HEADER {
            continue;
        }
        let arrived = now_ns();
        let frame = u32::from_be_bytes(buf[..4].try_into().unwrap());
        let count = u16::from_be_bytes(buf[6..8].try_into().unwrap());
        let key = buf[8] == 1;
        let sent = u64::from_be_bytes(buf[9..17].try_into().unwrap());
        let entry = partial.entry(frame).or_insert((count, 0));
        entry.1 += 1;
        if entry.1 < entry.0 {
            continue;
        }
        partial.remove(&frame);
        newest = newest.max(frame);
        partial.retain(|f, _| f.saturating_add(240) > newest);
        let decodable = key || (frame > 0 && last_ok == Some(frame - 1));
        if decodable {
            last_ok = Some(frame);
        }
        if key {
            asked = None;
        }
        lines.line(&format!("{frame},{},{sent},{arrived},{}", key as u8, decodable as u8));
        if !decodable && asked.is_none_or(|at| at.elapsed() >= KEY_ASK_AGAIN) {
            asked = Some(Instant::now());
            let _ = channel.send(b"K");
        }
    }
}

/// Each message: number, tag, sent, arrived.
fn receive_stamped(incoming: Incoming, lines: &Lines) {
    let mut buf = vec![0u8; 1 << 16];
    while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
        if n == 0 {
            break;
        }
        if n < HEADER {
            continue;
        }
        let arrived = now_ns();
        let number = u32::from_be_bytes(buf[..4].try_into().unwrap());
        let sent = u64::from_be_bytes(buf[4..12].try_into().unwrap());
        lines.line(&format!("{number},{},{sent},{arrived}", buf[12]));
    }
}

fn tablet(value: &dyn Fn(&str) -> Option<String>, node: Node, peer: String, out: &Path) {
    let events = Lines::create(out, "events.jsonl");
    let files: Vec<Arc<Lines>> = ["got-video.csv", "got-audio.csv", "got-control.csv", "sent-input.csv"].iter()
        .map(|name| Lines::create(out, name)).collect();
    watch(&node, events.clone(), None, None);
    resources(events, files.clone());
    let video = files[0].clone();
    node.serve("video", 1, move |incoming| receive_video(incoming, &video));
    for (feature, lines) in ["audio", "control"].into_iter().zip(files[1..3].iter().cloned()) {
        node.serve(feature, 1, move |incoming| receive_stamped(incoming, &lines));
    }
    // The pen, on each session the PC makes.
    let (pen, lines) = (node.clone(), files[3].clone());
    std::thread::spawn(move || send_stamped(pen, peer, Flow::new("input", Class::Input), Duration::from_micros(3500), 20,
                                            lines));
    let listen = value("--listen").unwrap_or_else(|| "0.0.0.0:47101".into()).parse().expect("an address");
    let bound = node.listen(listen).unwrap();
    node.listen_quic(bound).unwrap();
    if let Some(socket) = value("--bt") {
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let node = node.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let node = node.clone();
                std::thread::spawn(move || {
                    stream_room(&stream);
                    if let Err(e) = node.accept_stream(Box::new(stream), "bt") {
                        eprintln!("a bt stream: no session ({e})");
                    }
                });
            }
        });
    }
    println!("listening {}", bound.port());
    let _ = std::io::stdout().flush();
    loop {
        std::thread::park();
    }
}

// -- the PC ----------------------------------------------------------------------

enum Target {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

struct Candidate {
    kind: String,
    target: Target,
}

/// One of the paths, made: whether the session (or a new one) was made on it.
fn dial(node: &Node, candidate: &Candidate, events: &Lines) -> bool {
    let begun = Instant::now();
    let made = match &candidate.target {
        Target::Tcp(address) => node.connect_as(*address, DIAL_WAIT, &candidate.kind).map(|_| ()),
        Target::Unix(socket) => UnixStream::connect(socket).map_err(|e| e.to_string()).and_then(|stream| {
            stream_room(&stream);
            node.connect_stream(Box::new(stream), &candidate.kind).map(|_| ())
        }),
    };
    events.line(&json!({"at": now_ns(), "ev": "dial", "kind": candidate.kind, "ok": made.is_ok(),
                        "ms": begun.elapsed().as_millis() as u64, "error": made.as_ref().err()}).to_string());
    events.flush();
    made.is_ok()
}

/// The paths made as plus-one-link makes them: the first in order that
/// answers while the session has none (woken as it loses its path, and
/// each DIAL_EVERY), the ones before a path of upgrade each UPGRADE_EVERY.
fn dialer(node: Node, peer: String, candidates: Vec<Candidate>, upgrade: Vec<String>, events: Arc<Lines>,
          woken: mpsc::Receiver<()>) {
    let mut upgraded = Instant::now();
    loop {
        let session = node.session(&peer).filter(|s| !s.is_closed());
        match session.and_then(|s| s.path()) {
            None => {
                for candidate in &candidates {
                    if dial(&node, candidate, &events) {
                        break;
                    }
                }
            }
            Some(kind) if upgrade.contains(&kind) && upgraded.elapsed() >= UPGRADE_EVERY => {
                upgraded = Instant::now();
                for candidate in candidates.iter().take_while(|c| c.kind != kind) {
                    if dial(&node, candidate, &events) {
                        break;
                    }
                }
            }
            Some(_) => {}
        }
        let _ = woken.recv_timeout(DIAL_EVERY);
    }
}

/// A flow's channel on the session there is now, opened again on a new
/// one.
struct Flow {
    feature: &'static str,
    class: Class,
    held: Option<(Arc<Session>, Channel)>,
}

impl Flow {
    fn new(feature: &'static str, class: Class) -> Flow {
        Flow { feature, class, held: None }
    }

    /// The channel, its session, and whether it was just opened; None while
    /// there is no session (or the open failed: tried again next time).
    fn channel(&mut self, node: &Node, peer: &str) -> Option<(&Channel, &Arc<Session>, bool)> {
        let session = node.session(peer).filter(|s| !s.is_closed());
        let stale = match (&self.held, &session) {
            (Some((held, _)), Some(now)) => !Arc::ptr_eq(held, now) || held.is_closed(),
            (Some(_), None) => true,
            (None, _) => false,
        };
        if stale {
            self.held = None;
        }
        let mut fresh = false;
        if self.held.is_none() {
            let session = session?;
            match session.open(self.feature, self.feature, self.class) {
                Ok(channel) => {
                    self.held = Some((session, channel));
                    fresh = true;
                }
                Err(e) => {
                    eprintln!("{}: not opened ({e})", self.feature);
                    return None;
                }
            }
        }
        self.held.as_ref().map(|(session, channel)| (channel, session, fresh))
    }

    /// The channel failed (the session over): opened again next time.
    fn failed(&mut self) {
        self.held = None;
    }
}

/// The next instant of a flow each period: never more than a period
/// behind now (a flow held up, by an open waiting, skips what it missed:
/// none of it sent at once after).
fn next_due(due: &mut Instant, period: Duration) {
    *due += period;
    let now = Instant::now();
    if *due + period < now {
        *due = now;
    }
    std::thread::sleep(due.saturating_duration_since(now));
}

/// A message of size: number, sent (now), tag; and when it was sent.
fn stamped(number: u32, tag: u8, size: usize) -> (Vec<u8>, u64) {
    let sent = now_ns();
    let mut message = number.to_be_bytes().to_vec();
    message.extend_from_slice(&sent.to_be_bytes());
    message.push(tag);
    message.resize(size.max(HEADER), 0);
    (message, sent)
}

/// Each period, a message of size on a flow: number, tag, sent, sent
/// whole (lines).
fn send_stamped(node: Node, peer: String, mut flow: Flow, period: Duration, size: usize, lines: Arc<Lines>) {
    let (mut due, mut number) = (Instant::now(), 0u32);
    loop {
        next_due(&mut due, period);
        let sent = match flow.channel(&node, &peer) {
            Some((channel, _, _)) => {
                let (message, at) = stamped(number, 0, size);
                let ok = channel.send(&message).is_ok();
                lines.line(&format!("{number},0,{at},{}", ok as u8));
                ok
            }
            None => {
                lines.line(&format!("{number},0,{},0", now_ns()));
                true
            }
        };
        if !sent {
            flow.failed();
        }
        number += 1;
    }
}

/// The sound: a packet each 5 ms, PCM or Opus as pcm.py's follow() picks
/// from the path's budget (looked at each 100 ms); codec: the one now.
fn send_sound(node: Node, peer: String, codec: Arc<AtomicU8>, lines: Arc<Lines>) {
    let mut flow = Flow::new("audio", Class::Realtime);
    let (mut due, mut number, mut looked, mut pcm_at) = (Instant::now(), 0u32, Instant::now(), None::<Instant>);
    loop {
        next_due(&mut due, Duration::from_millis(5));
        let channel = flow.channel(&node, &peer);
        if looked.elapsed() >= Duration::from_millis(100) {
            looked = Instant::now();
            let kbps = channel.as_ref().and_then(|(_, session, _)| session.rate()).map(|rate| rate * 8 / 1000);
            let opus = codec.load(Ordering::Relaxed) == 1;
            match kbps {
                Some(kbps) if kbps < OPUS_UNDER => {
                    pcm_at = None;
                    codec.store(1, Ordering::Relaxed);
                }
                _ if opus && kbps.is_none_or(|kbps| kbps >= PCM_OVER) => {
                    pcm_at = pcm_at.or(Some(Instant::now() + PCM_AFTER));
                }
                _ => pcm_at = None,
            }
        }
        if pcm_at.is_some_and(|at| Instant::now() >= at) {
            pcm_at = None;
            codec.store(0, Ordering::Relaxed);
        }
        let tag = codec.load(Ordering::Relaxed);
        let sent = match channel {
            Some((channel, _, _)) => {
                let (message, at) = stamped(number, tag, (if tag == 1 { OPUS } else { PCM }) + HEADER);
                let ok = channel.send(&message).is_ok();
                lines.line(&format!("{number},{tag},{at},{}", ok as u8));
                ok
            }
            None => {
                lines.line(&format!("{number},{tag},{},0", now_ns()));
                true
            }
        };
        if !sent {
            flow.failed();
        }
        number += 1;
    }
}

/// The video: 60 frames a second in MAX_DATAGRAM pieces; lines: frame,
/// keyframe, why (0 none, 1 a new channel, 2 asked, 3 periodic), sent,
/// bytes, sent whole.
fn send_video(node: Node, peer: String, kbps: u64, codec: Arc<AtomicU8>, lines: Arc<Lines>) {
    let mut flow = Flow::new("video", Class::Realtime);
    let asked = Arc::new(AtomicBool::new(false));
    let mut stop = Arc::new(AtomicBool::new(false));
    let payload = MAX_DATAGRAM - VIDEO_HEADER;
    let (mut due, mut frame, mut last_key) = (Instant::now(), 0u32, None::<Instant>);
    loop {
        next_due(&mut due, Duration::from_nanos(1_000_000_000 / FPS));
        let Some((channel, session, fresh)) = flow.channel(&node, &peer) else {
            lines.line(&format!("{frame},0,0,{},0,0", now_ns()));
            frame += 1;
            continue;
        };
        if fresh {
            // The tablet's asking for keyframes, read until the channel ends.
            stop.store(true, Ordering::Relaxed);
            stop = Arc::new(AtomicBool::new(false));
            let (reader, asked, stop) = (channel.try_clone().unwrap(), asked.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 64];
                while !stop.load(Ordering::Relaxed) {
                    match reader.recv(&mut buf, Some(Duration::from_millis(200))) {
                        Ok(Some(0)) | Err(_) => break,
                        Ok(Some(_)) => asked.store(true, Ordering::Relaxed),
                        Ok(None) => {}
                    }
                }
            });
        }
        let share = if codec.load(Ordering::Relaxed) == 1 { OPUS_SHARE } else { PCM_SHARE };
        let rate = match session.rate() {
            Some(rate) => (rate * 85 / 100).saturating_sub(share).clamp(12_500, 2_500_000),
            None => kbps * 1000 / 8,
        };
        let gap = session.rtt().map_or(KEY_GAP, |rtt| KEY_GAP.max(rtt * 3 / 2 + Duration::from_millis(4000 / FPS)));
        let since = last_key.map(|at| at.elapsed());
        let why = if fresh {
            1
        } else if asked.load(Ordering::Relaxed) && since.is_none_or(|since| since >= gap) {
            2
        } else if since.is_none_or(|since| since >= KEY_EVERY) {
            3
        } else {
            0
        };
        let key = why != 0;
        if key {
            asked.store(false, Ordering::Relaxed);
            last_key = Some(Instant::now());
        }
        let average = (rate / FPS) as usize;
        let size = if key { average * 4 } else { average }.max(1);
        let count = size.div_ceil(payload);
        let sent = now_ns();
        let mut ok = true;
        for index in 0..count {
            let mut packet = frame.to_be_bytes().to_vec();
            packet.extend_from_slice(&(index as u16).to_be_bytes());
            packet.extend_from_slice(&(count as u16).to_be_bytes());
            packet.push(key as u8);
            packet.extend_from_slice(&sent.to_be_bytes());
            packet.resize(VIDEO_HEADER + payload.min(size - index * payload), 0);
            ok &= channel.send(&packet).is_ok();
        }
        lines.line(&format!("{frame},{},{why},{sent},{size},{}", key as u8, ok as u8));
        if !ok {
            flow.failed();
        }
        frame += 1;
    }
}

fn pc(value: &dyn Fn(&str) -> Option<String>, args: &[String], node: Node, peer: String, out: &Path) {
    let candidates: Vec<Candidate> = args.windows(2).filter(|pair| pair[0] == "--path").map(|pair| {
        let (kind, target) = pair[1].split_once('=').expect("--path KIND=ADDRESS");
        let target = match target.strip_prefix("unix:") {
            Some(socket) => Target::Unix(socket.into()),
            None => Target::Tcp(target.parse().expect("an address")),
        };
        Candidate { kind: kind.into(), target }
    }).collect();
    assert!(!candidates.is_empty(), "--path KIND=ADDRESS");
    let upgrade: Vec<String> = value("--upgrade-from").map(|kinds| kinds.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let kbps: u64 = value("--video").and_then(|v| v.parse().ok()).unwrap_or(8000);
    node.upgrade_to_quic(args.iter().any(|a| a == "--quic"));
    let events = Lines::create(out, "events.jsonl");
    let files: Vec<Arc<Lines>> = ["sent-video.csv", "sent-audio.csv", "sent-control.csv", "got-input.csv"].iter()
        .map(|name| Lines::create(out, name)).collect();
    let (wake, woken) = mpsc::channel();
    watch(&node, events.clone(), Some(out.join("current")), Some(wake));
    resources(events.clone(), files.clone());
    let input = files[3].clone();
    node.serve("input", 1, move |incoming| receive_stamped(incoming, &input));
    // The first path, before the load.
    let started = Instant::now();
    while node.session(&peer).is_none() {
        assert!(started.elapsed() < Duration::from_secs(60), "no session in 60 s");
        if !candidates.iter().any(|candidate| dial(&node, candidate, &events)) {
            std::thread::sleep(DIAL_EVERY);
        }
    }
    let codec = Arc::new(AtomicU8::new(0));
    let spawn = |name: &str, run: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new().name(name.into()).spawn(run).unwrap();
    };
    {
        let (node, peer, codec, lines) = (node.clone(), peer.clone(), codec.clone(), files[0].clone());
        spawn("video", Box::new(move || send_video(node, peer, kbps, codec, lines)));
    }
    {
        let (node, peer, codec, lines) = (node.clone(), peer.clone(), codec.clone(), files[1].clone());
        spawn("audio", Box::new(move || send_sound(node, peer, codec, lines)));
    }
    {
        let (node, peer, lines) = (node.clone(), peer.clone(), files[2].clone());
        spawn("control", Box::new(move || send_stamped(node, peer, Flow::new("control", Class::Control),
                                                       Duration::from_millis(50), 64, lines)));
    }
    println!("loaded");
    let _ = std::io::stdout().flush();
    dialer(node, peer, candidates, upgrade, events, woken);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let role = args.get(1).map(String::as_str).unwrap_or("");
    if role == "now" {
        println!("{}", now_ns());
        return;
    }
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let identity = Identity::from_pem_files(&value("--cert").expect("--cert"), &value("--key").expect("--key")).unwrap();
    let trusted = value("--trust").expect("--trust");
    let out = PathBuf::from(value("--out").expect("--out"));
    let peer = trusted.clone();
    match role {
        "tablet" => {
            let node = Node::new("Bench tablet", &identity, Arc::new(move |fp: &str| fp == trusted)).unwrap();
            tablet(&value, node, peer, &out);
        }
        "pc" => {
            let node = Node::new("Bench PC", &identity, Arc::new(move |fp: &str| fp == trusted)).unwrap();
            pc(&value, &args, node, peer, &out);
        }
        _ => {
            eprintln!("switchbench tablet|pc|now ...");
            std::process::exit(2);
        }
    }
}
