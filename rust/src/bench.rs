//! A tablet of pretend, on this PC, for measuring the PC's side alone (no
//! Wi-Fi in between): it answers as the app does (TLS, hello, the display
//! request), takes the desktop's packets, checks and puts each frame back
//! together, and reports each second as DisplaySession.kt does, so the PC's
//! rate control runs as with the tablet. On the same machine the PC's clock
//! is its own: it measures each frame from its capture to its last packet in.
//!
//!   plus-one-rs fake-app DIR [--port P] [--screen WxH]
//! writes DIR/fingerprint (the PC pairs with it through a paired.json of its
//! own), serves one display session, then prints what it measured.

use crate::link::datagrams::{FROM_APP, FROM_PC, NONCE, TAG};
use crate::link::trust::{fingerprint, hex, unhex};
use crate::link::wire::{self, MAX_HEADER, UNPAIRED_HEADER};
use crate::msg;
use openssl::asn1::Asn1Time;
use openssl::cipher::Cipher;
use openssl::cipher_ctx::CipherCtx;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::ssl::{SslAcceptor, SslMethod, SslVerifyMode};
use openssl::x509::{X509NameBuilder, X509};
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn now_ns() -> i64 {
    crate::display::streamer::monotonic_ns()
}

struct Frame {
    count: usize,
    have: usize,
    captured: i64,
    bytes: usize,
}

#[derive(Default)]
struct Measured {
    latencies: Vec<f64>,  // ms, capture to the frame's last packet in
    second: Vec<f64>,
    frames: u64,
    lost: u64,
    lost_second: u32,
    bytes: u64,
    packets: u64,
}

pub fn fake_app(args: &[String]) {
    let dir = std::path::PathBuf::from(args.first().expect("a directory"));
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = value("--port").and_then(|v| v.parse().ok()).unwrap_or(47999);
    let (width, height) = value("--screen").and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
    }).unwrap_or((2880, 1800));

    // Its own certificate, as the app's: the PC pairs with its fingerprint.
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "Plus One bench tablet").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    let (from, to) = (Asn1Time::days_from_now(0).unwrap(), Asn1Time::days_from_now(1).unwrap());
    cert.set_not_before(&from).unwrap();
    cert.set_not_after(&to).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fingerprint"), fingerprint(&cert.to_der().unwrap())).unwrap();

    let mut tls = SslAcceptor::mozilla_modern_v5(SslMethod::tls_server()).unwrap();
    tls.set_private_key(&key).unwrap();
    tls.set_certificate(&cert).unwrap();
    tls.set_verify_callback(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT, |_, _| true);
    let tls = Arc::new(tls.build());
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let seconds: f64 = value("--seconds").and_then(|v| v.parse().ok()).unwrap_or(30.0);
    eprintln!("fake app: at 127.0.0.1:{port}, screen {width}x{height}, for {seconds} s");
    // Each connection on a thread of its own, as the app's LinkServer; what
    // each session measured, said when it ends.
    let reports: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut sessions = Vec::new();
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((tcp, _)) => {
                tcp.set_nonblocking(false).unwrap();
                let (tls, reports) = (tls.clone(), reports.clone());
                sessions.push(std::thread::spawn(move || {
                    if let Some(report) = session(&tls, tcp, (width, height)) {
                        reports.lock().unwrap().push(report);
                    }
                }));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    for session in sessions {
        let _ = session.join();
    }
    for report in reports.lock().unwrap().iter() {
        println!("{report}");
    }
}

/// One connection: hello, then its request (the desktop, the speakers).
fn session(tls: &SslAcceptor, tcp: std::net::TcpStream, screen: (u32, u32)) -> Option<String> {
    let mut stream = tls.accept(tcp).ok()?;
    let hello = wire::receive(&mut stream, UNPAIRED_HEADER).ok()?;
    if hello.get("type").and_then(Value::as_str) != Some("hello") {
        return None;
    }
    wire::send(&mut stream, &msg!({"type": "hello", "name": "Bench", "version": 3, "paired": true,
                                   "at": "127.0.0.1", "app": 900}), None).ok()?;
    let request = wire::receive(&mut stream, MAX_HEADER).ok()?;
    match request.get("type").and_then(Value::as_str) {
        Some("display") => Some(display(stream, &request, screen)),
        Some("speakers") => Some(speakers(stream, &request)),
        _ => None,
    }
}

/// The link, until the PC says stop or hangs up.
fn link_until_stop(mut stream: openssl::ssl::SslStream<std::net::TcpStream>, done: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while let Ok(message) = wire::receive(&mut stream, MAX_HEADER) {
            if message.get("type").and_then(Value::as_str) == Some("stop") {
                break;
            }
        }
        done.store(true, Ordering::Relaxed);
    });
}

fn opener(key: &[u8]) -> CipherCtx {
    let mut opener = CipherCtx::new().unwrap();
    opener.decrypt_init(Some(Cipher::aes_256_gcm()), Some(key), None).unwrap();
    opener
}

/// The PC's packet's body, or None if it is not one sealed with the key.
fn open(opener: &mut CipherCtx, packet: &[u8]) -> Option<Vec<u8>> {
    let n = packet.len();
    if n < NONCE + TAG + 1 || u32::from_be_bytes(packet[..4].try_into().unwrap()) != FROM_PC {
        return None;
    }
    opener.decrypt_init(None, None, Some(&packet[..NONCE])).ok()?;
    opener.set_tag(&packet[n - TAG..n]).ok()?;
    let mut body = vec![0u8; n - NONCE - TAG + 16];
    let k = opener.cipher_update(&packet[NONCE..n - TAG], Some(&mut body)).ok()?;
    let f = opener.cipher_final(&mut body[k..]).ok()?;
    body.truncate(k + f);
    Some(body)
}

/// The speakers: each 5 ms packet's arrival against when it was due (the
/// first one's arrival, then 5 ms each): how late the sound comes, and how
/// unevenly, while the rest of the PC's side is busy.
fn speakers(mut stream: openssl::ssl::SslStream<std::net::TcpStream>, request: &wire::Message) -> String {
    let offer: Value = serde_json::from_str(request.get("sdp").and_then(Value::as_str).unwrap_or("{}")).unwrap();
    let key = unhex(offer.get("key").and_then(Value::as_str).unwrap()).unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let answer = serde_json::json!({"port": udp.local_addr().unwrap().port()}).to_string();
    wire::send(&mut stream, &msg!({"type": "answer", "sdp": answer}), None).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    link_until_stop(stream, done.clone());
    let mut opener = opener(&key);
    let mut packet = [0u8; 2048];
    let mut first: Option<(i64, u32)> = None;
    let mut lateness = Vec::new();  // ms past due
    let mut previous: Option<i64> = None;
    let mut gaps = 0;  // more than 20 ms between two packets
    while !done.load(Ordering::Relaxed) {
        let Ok((n, _)) = udp.recv_from(&mut packet) else { continue };
        let arrived = now_ns();
        let Some(body) = open(&mut opener, &packet[..n]) else { continue };
        if body.len() < 5 || body[0] != 7 {
            continue;
        }
        let number = u32::from_be_bytes(body[1..5].try_into().unwrap());
        let (t0, n0) = *first.get_or_insert((arrived, number));
        let due = t0 + (number.wrapping_sub(n0) as i64) * 5_000_000;
        lateness.push((arrived - due) as f64 / 1e6);
        if previous.is_some_and(|p| arrived - p > 20_000_000) {
            gaps += 1;
        }
        previous = Some(arrived);
    }
    // Relative to the earliest: the sender's own pace drifts against nothing here.
    let least = lateness.iter().copied().fold(f64::INFINITY, f64::min);
    let mut late: Vec<f64> = lateness.iter().map(|l| l - least).collect();
    late.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| late.get(((late.len() as f64 * p) as usize).min(late.len().saturating_sub(1))).copied().unwrap_or(0.0);
    format!("speakers: {} packets; late against the steady 5 ms: p50 {:.2} ms, p99 {:.2}, p99.9 {:.2}, max {:.2}; \
             {gaps} gaps over 20 ms", late.len(), q(0.5), q(0.99), q(0.999), late.last().copied().unwrap_or(0.0))
}

/// The desktop: frames put back together, each one's capture to its last
/// packet in, reports each second as the app's.
fn display(mut stream: openssl::ssl::SslStream<std::net::TcpStream>, request: &wire::Message, (width, height): (u32, u32)) -> String {
    let key = unhex(request.get("key").and_then(Value::as_str).unwrap()).unwrap();
    let pc_port = request.get("port").and_then(Value::as_u64).unwrap() as u16;
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    // As DisplaySession.kt: room for a keyframe's packets at once.
    crate::link::datagrams::set_int(&udp, libc::SOL_SOCKET, libc::SO_RCVBUF, 4 << 20);
    udp.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    wire::send(&mut stream, &msg!({"type": "display-ready", "port": udp.local_addr().unwrap().port(),
                                   "width": width, "height": height, "refresh": 60.0}), None).unwrap();
    let pc = std::net::SocketAddr::from(([127, 0, 0, 1], pc_port));
    let done = Arc::new(AtomicBool::new(false));
    link_until_stop(stream, done.clone());

    let measured = Arc::new(Mutex::new(Measured::default()));
    let mut opener = opener(&key);
    let mut sealer = CipherCtx::new().unwrap();
    sealer.encrypt_init(Some(Cipher::aes_256_gcm()), Some(&key), None).unwrap();
    let mut sent = 0u64;
    let mut seal = |body: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(NONCE + body.len() + TAG);
        out.extend_from_slice(&FROM_APP.to_be_bytes());
        out.extend_from_slice(&sent.to_be_bytes());
        sent += 1;
        let nonce: [u8; 12] = out[..12].try_into().unwrap();
        sealer.encrypt_init(None, None, Some(&nonce)).unwrap();
        let mut sealed = vec![0u8; body.len()];
        let n = sealer.cipher_update(body, Some(&mut sealed)).unwrap();
        sealer.cipher_final(&mut sealed[n..]).unwrap();
        let mut tag = [0u8; 16];
        sealer.tag(&mut tag).unwrap();
        out.extend_from_slice(&sealed);
        out.extend_from_slice(&tag);
        out
    };
    let mut frames: BTreeMap<u32, Frame> = BTreeMap::new();
    let mut newest = 0u32;
    let mut packet = [0u8; 2048];
    let mut second = Instant::now();
    let started = Instant::now();
    while !done.load(Ordering::Relaxed) {
        if second.elapsed() >= Duration::from_secs(1) {
            second = Instant::now();
            // A ping (shown: the desktop is on screen) and the second's report.
            let mut ping = vec![3u8];
            ping.extend_from_slice(&now_ns().to_be_bytes());
            ping.push(1);
            let _ = udp.send_to(&seal(&ping), pc);
            let mut m = measured.lock().unwrap();
            let mut s = std::mem::take(&mut m.second);
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let pick = |q: f64| s.get(((s.len() as f64 * q) as usize).min(s.len().saturating_sub(1))).copied().unwrap_or(0.0);
            let tenths = |v: f64| ((v * 10.0).min(65535.0)) as u16;
            let mut stats = vec![5u8];
            stats.extend_from_slice(&(s.len() as u32).to_be_bytes());
            stats.extend_from_slice(&std::mem::take(&mut m.lost_second).to_be_bytes());
            for v in [pick(0.5), pick(0.9), s.last().copied().unwrap_or(0.0), 1.0, 1.0, 1.0] {
                stats.extend_from_slice(&tenths(v).to_be_bytes());
            }
            drop(m);
            let _ = udp.send_to(&seal(&stats), pc);
        }
        let Ok((n, _)) = udp.recv_from(&mut packet) else { continue };
        let arrived = now_ns();
        let Some(body) = open(&mut opener, &packet[..n]) else { continue };
        if body.len() < 18 || body[0] != 1 {
            continue;
        }
        let number = u32::from_be_bytes(body[2..6].try_into().unwrap());
        let count = u16::from_be_bytes(body[8..10].try_into().unwrap()) as usize;
        let captured = i64::from_be_bytes(body[10..18].try_into().unwrap());
        if number <= newest {
            continue;
        }
        let frame = frames.entry(number).or_insert(Frame { count, have: 0, captured, bytes: 0 });
        frame.have += 1;
        frame.bytes += body.len() - 18;
        let mut m = measured.lock().unwrap();
        m.packets += 1;
        if frame.have == frame.count {
            let bytes = frame.bytes;
            let older: Vec<u32> = frames.range(..number).map(|(k, _)| *k).collect();
            m.lost += older.len() as u64;
            m.lost_second += older.len() as u32;
            for k in older {
                frames.remove(&k);
            }
            frames.remove(&number);
            newest = number;
            let ms = (arrived - captured) as f64 / 1e6;
            m.latencies.push(ms);
            m.second.push(ms);
            m.frames += 1;
            m.bytes += bytes as u64;
        }
    }
    let m = measured.lock().unwrap();
    let mut l = m.latencies.clone();
    l.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| l.get(((l.len() as f64 * p) as usize).min(l.len().saturating_sub(1))).copied().unwrap_or(0.0);
    let seconds = started.elapsed().as_secs_f64();
    let _ = hex;
    format!("display: frames {} ({:.1}/s), lost {}, {:.1} Mbit/s, {} packets; capture to last packet in: \
             p50 {:.2} ms, p90 {:.2}, p99 {:.2}, max {:.2}",
            m.frames, m.frames as f64 / seconds, m.lost, m.bytes as f64 * 8.0 / seconds / 1e6, m.packets,
            q(0.5), q(0.9), q(0.99), l.last().copied().unwrap_or(0.0))
}
