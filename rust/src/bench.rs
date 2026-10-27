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
    let tls = tls.build();
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    eprintln!("fake app: at 127.0.0.1:{port}, screen {width}x{height}");

    let (tcp, _) = listener.accept().unwrap();
    let mut stream = tls.accept(tcp).expect("TLS");
    let hello = wire::receive(&mut stream, UNPAIRED_HEADER).unwrap();
    assert_eq!(hello.get("type").and_then(Value::as_str), Some("hello"));
    wire::send(&mut stream, &msg!({"type": "hello", "name": "Bench", "version": 3, "paired": true,
                                   "at": "127.0.0.1", "app": 900}), None).unwrap();
    let request = wire::receive(&mut stream, MAX_HEADER).unwrap();
    assert_eq!(request.get("type").and_then(Value::as_str), Some("display"));
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
    // The link: until the PC says stop or hangs up.
    {
        let done = done.clone();
        std::thread::spawn(move || {
            while let Ok(message) = wire::receive(&mut stream, MAX_HEADER) {
                if message.get("type").and_then(Value::as_str) == Some("stop") {
                    break;
                }
            }
            done.store(true, Ordering::Relaxed);
        });
    }

    let measured = Arc::new(Mutex::new(Measured::default()));
    let mut opener = CipherCtx::new().unwrap();
    opener.decrypt_init(Some(Cipher::aes_256_gcm()), Some(&key), None).unwrap();
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
        if n < NONCE + TAG + 1 || u32::from_be_bytes(packet[..4].try_into().unwrap()) != FROM_PC {
            continue;
        }
        opener.decrypt_init(None, None, Some(&packet[..NONCE])).unwrap();
        opener.set_tag(&packet[n - TAG..n]).unwrap();
        let mut body = vec![0u8; n - NONCE - TAG + 16];
        let Ok(k) = opener.cipher_update(&packet[NONCE..n - TAG], Some(&mut body)) else { continue };
        let Ok(f) = opener.cipher_final(&mut body[k..]) else { continue };  // not sealed with the key
        body.truncate(k + f);
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
    println!("frames {} ({:.1}/s), lost {}, {:.1} Mbit/s, {} packets; capture to last packet in: \
              p50 {:.2} ms, p90 {:.2}, p99 {:.2}, max {:.2}",
             m.frames, m.frames as f64 / seconds, m.lost, m.bytes as f64 * 8.0 / seconds / 1e6, m.packets,
             q(0.5), q(0.9), q(0.99), l.last().copied().unwrap_or(0.0));
    let _ = hex;
}
