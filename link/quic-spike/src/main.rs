//! qbench: Stage 1's QUIC spike (architecture §3.14). quinn on the same
//! networks as today's link, measured the same way: a stream's bulk one way
//! and both ways, datagram round trips idle and under bulk, and a
//! video-like datagram flow, at a fixed bitrate or following the congestion
//! controller's estimate (cwnd / rtt). The tablet runs the server (adb
//! shell), the PC the client.
//!
//! A measurement tool, not the link: no pairing, and the client does not
//! check the server's certificate (nothing private goes over it).
//!
//!   qbench server PORT CC                       CC: newreno | cubic | bbr | gcc | copa[:DELTA]
//!                                             | ledbat[:TARGET_MS[:GAIN]]
//!   qbench client HOST:PORT CC SECONDS TEST...  TEST: rt | up | down | both
//!                                               | rt+both | video:KBPS | video:auto
//!                                               | video:auto+up

mod copa;
mod gcc;
mod ledbat;

use bytes::Bytes;
use quinn::{congestion, ClientConfig, Connection, Endpoint, ServerConfig, TransportConfig};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const ECHO: u8 = b'E';
const FRAME: u8 = b'V';
const FRAME_ACK: u8 = b'A';
const FPS: u64 = 60;
const KEYFRAME_EVERY: u64 = 2 * FPS;
const KEYFRAME_SIZE: u64 = 4; // average frames
const FRAME_HEADER: usize = 1 + 4 + 2 + 2;

fn transport(cc: &str, rate: Arc<AtomicU64>) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.stream_receive_window((16u32 << 20).into());
    t.receive_window((32u32 << 20).into());
    // Little more than a fast path's bandwidth-delay product: a test that
    // stops writing ends then, not after a buffer's worth more.
    t.send_window(4 << 20);
    t.datagram_receive_buffer_size(Some(4 << 20));
    // What may wait for the congestion window: little, realtime late is lost.
    // A keyframe at 20 Mbit/s (about 170 KB) fits.
    t.datagram_send_buffer_size(512 << 10);
    // QBENCH_MTU: the path's MTU known (a local network's 1500, less IP and
    // UDP): from the first packet, not after quinn's search finds it.
    if let Some(mtu) = std::env::var("QBENCH_MTU").ok().and_then(|m| m.parse().ok()) {
        t.initial_mtu(mtu);
    }
    // QBENCH_ACKS: how many packets the peer may take before it must acknowledge.
    if let Some(acks) = std::env::var("QBENCH_ACKS").ok().and_then(|a| a.parse::<u32>().ok()) {
        let mut ack = quinn::AckFrequencyConfig::default();
        ack.ack_eliciting_threshold(acks.into());
        t.ack_frequency_config(Some(ack));
    }
    match cc {
        "newreno" => t.congestion_controller_factory(Arc::new(congestion::NewRenoConfig::default())),
        "bbr" => t.congestion_controller_factory(Arc::new(congestion::BbrConfig::default())),
        "gcc" => {
            // The peer acknowledges every second packet, within 2 ms: the
            // acknowledgements' times stand for the arrivals.
            let mut ack = quinn::AckFrequencyConfig::default();
            ack.ack_eliciting_threshold(1u32.into()).max_ack_delay(Some(Duration::from_millis(2)));
            t.ack_frequency_config(Some(ack));
            t.congestion_controller_factory(Arc::new(gcc::GccConfig { rate }))
        }
        c if c.starts_with("ledbat") => {
            // ledbat[:TARGET_MS[:GAIN]]
            let mut parts = c.split(':').skip(1);
            let target = parts.next().and_then(|t| t.parse().ok()).unwrap_or(10.0);
            let gain = parts.next().and_then(|g| g.parse().ok()).unwrap_or(4.0);
            t.congestion_controller_factory(Arc::new(ledbat::LedbatConfig {
                target: Duration::from_secs_f64(target / 1000.0), gain }))
        }
        c if c.starts_with("copa") => {
            let delta = c.strip_prefix("copa:").and_then(|d| d.parse().ok()).unwrap_or(0.5);
            t.congestion_controller_factory(Arc::new(copa::CopaConfig { delta }))
        }
        _ => t.congestion_controller_factory(Arc::new(congestion::CubicConfig::default())),
    };
    t
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct AnyServer(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyServer {
    fn verify_server_cert(&self, _: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>],
                          _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime)
                          -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>,
                              dss: &rustls::DigitallySignedStruct)
                              -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>,
                              dss: &rustls::DigitallySignedStruct)
                              -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Realtime: sent only if what already waits for the window is under
/// 100 ms at the controller's rate (late is lost); else dropped here.
fn send_realtime(conn: &Connection, rate: &AtomicU64, data: Bytes) -> bool {
    let rate = rate.load(Ordering::Relaxed);
    if rate > 0 {
        let waiting = (512usize << 10).saturating_sub(conn.datagram_send_buffer_space());
        if waiting as u64 > rate / 10 {
            return false;
        }
    }
    conn.send_datagram(data).is_ok()
}

fn now_us(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

fn path_stats(conn: &Connection) -> String {
    let s = conn.stats();
    format!("rtt {:.1} ms, cwnd {} KB, lost {} packets, congestion events {}, sent {} packets, mtu {}, {} UDP datagrams in {} sends",
            s.path.rtt.as_secs_f64() * 1000.0, s.path.cwnd >> 10, s.path.lost_packets, s.path.congestion_events,
            s.path.sent_packets, s.path.current_mtu, s.udp_tx.datagrams, s.udp_tx.ios)
}

// -- server -------------------------------------------------------------------------

async fn server(port: u16, cc: String) {
    let cert = rcgen::generate_simple_self_signed(vec!["plusone".into()]).expect("a certificate");
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into()).unwrap();
    let mut config = ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap()));
    let rate: Arc<AtomicU64> = Arc::default();
    config.transport_config(Arc::new(transport(&cc, rate.clone())));
    let endpoint = Endpoint::server(config, SocketAddr::from(([0, 0, 0, 0], port))).expect("listening");
    eprintln!("qbench server on {port}, {cc}");
    while let Some(incoming) = endpoint.accept().await {
        let rate = rate.clone();
        tokio::spawn(async move {
            if let Ok(conn) = incoming.await {
                serve(conn, rate).await;
            }
        });
    }
}

async fn serve(conn: Connection, rate: Arc<AtomicU64>) {
    let datagrams = conn.clone();
    tokio::spawn(async move {
        // Frames being put together: frame -> (pieces, pieces come).
        let mut frames: HashMap<u32, (u16, u16)> = HashMap::new();
        while let Ok(d) = datagrams.read_datagram().await {
            match d.first() {
                Some(&ECHO) => {
                    send_realtime(&datagrams, &rate, d);
                }
                Some(&FRAME) if d.len() >= FRAME_HEADER => {
                    let frame = u32::from_be_bytes(d[1..5].try_into().unwrap());
                    let count = u16::from_be_bytes(d[7..9].try_into().unwrap());
                    let entry = frames.entry(frame).or_insert((count, 0));
                    entry.1 += 1;
                    if entry.1 == entry.0 {
                        frames.remove(&frame);
                        let mut ack = vec![FRAME_ACK];
                        ack.extend_from_slice(&frame.to_be_bytes());
                        send_realtime(&datagrams, &rate, Bytes::from(ack));
                    }
                    frames.retain(|&f, _| f + 120 > frame);
                }
                _ => {}
            }
        }
    });
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let conn = conn.clone();
        tokio::spawn(async move {
            let mut first = [0u8; 9];
            if recv.read_exact(&mut first).await.is_err() {
                return;
            }
            let seconds = u64::from_be_bytes(first[1..9].try_into().unwrap());
            match first[0] {
                b'U' => {
                    // Up: count until its end; say how much, and in how long.
                    let mut buf = vec![0u8; 256 << 10];
                    let (mut bytes, mut started, mut last) = (0u64, None, Instant::now());
                    while let Ok(Some(n)) = recv.read(&mut buf).await {
                        started.get_or_insert_with(Instant::now);
                        bytes += n as u64;
                        last = Instant::now();
                    }
                    let us = started.map(|s| last.duration_since(s).as_micros() as u64).unwrap_or(0);
                    let mut reply = bytes.to_be_bytes().to_vec();
                    reply.extend_from_slice(&us.to_be_bytes());
                    let _ = send.write_all(&reply).await;
                    let _ = send.finish();
                }
                b'D' => {
                    let piece = vec![0u8; 64 << 10];
                    let until = Instant::now() + Duration::from_secs(seconds);
                    while Instant::now() < until {
                        if send.write_all(&piece).await.is_err() {
                            break;
                        }
                    }
                    let _ = send.reset(0u32.into());
                }
                b'S' => {
                    let _ = send.write_all(path_stats(&conn).as_bytes()).await;
                    let _ = send.finish();
                }
                _ => {}
            }
        });
    }
}

// -- client -------------------------------------------------------------------------

struct Client {
    conn: Connection,
    start: Instant,
    echoes: Mutex<Option<mpsc::UnboundedSender<u64>>>,
    acks: Mutex<Option<mpsc::UnboundedSender<u32>>>,
    /// The controller's rate (bytes a second; gcc only, else 0).
    rate: Arc<AtomicU64>,
}

impl Client {
    async fn connect(address: SocketAddr, cc: &str) -> Arc<Client> {
        let tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
            .dangerous().with_custom_certificate_verifier(Arc::new(AnyServer(provider())))
            .with_no_client_auth();
        let mut config = ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap()));
        let rate: Arc<AtomicU64> = Arc::default();
        config.transport_config(Arc::new(transport(cc, rate.clone())));
        let mut endpoint = Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0))).expect("a socket");
        endpoint.set_default_client_config(config);
        let t0 = Instant::now();
        let conn = endpoint.connect(address, "plusone").unwrap().await.expect("connected");
        println!("connected in {:.1} ms, max datagram {:?}", t0.elapsed().as_secs_f64() * 1000.0, conn.max_datagram_size());
        let client = Arc::new(Client { conn, start: Instant::now(), echoes: Mutex::new(None), acks: Mutex::new(None), rate });
        let me = client.clone();
        tokio::spawn(async move {
            while let Ok(d) = me.conn.read_datagram().await {
                match d.first() {
                    Some(&ECHO) if d.len() >= 9 => {
                        if let Some(tx) = me.echoes.lock().unwrap().as_ref() {
                            let _ = tx.send(u64::from_be_bytes(d[1..9].try_into().unwrap()));
                        }
                    }
                    Some(&FRAME_ACK) if d.len() >= 5 => {
                        if let Some(tx) = me.acks.lock().unwrap().as_ref() {
                            let _ = tx.send(u32::from_be_bytes(d[1..5].try_into().unwrap()));
                        }
                    }
                    _ => {}
                }
            }
        });
        client
    }

    async fn their_stats(&self) -> String {
        let Ok((mut send, mut recv)) = self.conn.open_bi().await else { return "?".into() };
        let mut head = vec![b'S'];
        head.extend_from_slice(&0u64.to_be_bytes());
        let _ = send.write_all(&head).await;
        let _ = send.finish();
        String::from_utf8_lossy(&recv.read_to_end(1024).await.unwrap_or_default()).into_owned()
    }

    async fn up(&self, seconds: u64) -> f64 {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        let mut head = vec![b'U'];
        head.extend_from_slice(&seconds.to_be_bytes());
        send.write_all(&head).await.unwrap();
        let piece = vec![0u8; 64 << 10];
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            if send.write_all(&piece).await.is_err() {
                break;
            }
        }
        let _ = send.reset(0u32.into());
        let reply = recv.read_to_end(64).await.unwrap_or_default();
        if reply.len() < 16 {
            return f64::NAN;
        }
        let bytes = u64::from_be_bytes(reply[..8].try_into().unwrap());
        let us = u64::from_be_bytes(reply[8..16].try_into().unwrap()).max(1);
        bytes as f64 / us as f64
    }

    async fn down(&self, seconds: u64) -> f64 {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        let mut head = vec![b'D'];
        head.extend_from_slice(&seconds.to_be_bytes());
        send.write_all(&head).await.unwrap();
        let mut buf = vec![0u8; 256 << 10];
        let (mut bytes, mut started, mut last) = (0u64, None, Instant::now());
        while let Ok(Some(n)) = recv.read(&mut buf).await {
            started.get_or_insert_with(Instant::now);
            bytes += n as u64;
            last = Instant::now();
        }
        let _ = send.finish();
        let us = started.map(|s| last.duration_since(s).as_micros() as u64).unwrap_or(1).max(1);
        bytes as f64 / us as f64
    }

    /// Frames at 60 fps, a keyframe every 2 s, each cut into datagrams; the
    /// server acknowledges each whole frame. kbps None: the bitrate follows
    /// the controller (0.8 of cwnd / rtt, every 100 ms).
    async fn video(&self, seconds: u64, kbps: Option<u64>) -> String {
        let (tx, mut rx) = mpsc::unbounded_channel();
        *self.acks.lock().unwrap() = Some(tx);
        let sent_at: Arc<Mutex<HashMap<u32, u64>>> = Arc::default();
        let latencies: Arc<Mutex<Vec<f64>>> = Arc::default();
        let (acked, start) = (Arc::new(AtomicU64::new(0)), self.start);
        let collector = {
            let (sent_at, latencies, acked) = (sent_at.clone(), latencies.clone(), acked.clone());
            tokio::spawn(async move {
                while let Some(frame) = rx.recv().await {
                    if let Some(t) = sent_at.lock().unwrap().remove(&frame) {
                        latencies.lock().unwrap().push((now_us(start) - t) as f64 / 1000.0);
                        acked.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        let payload = self.conn.max_datagram_size().unwrap_or(1200).min(1400) - FRAME_HEADER;
        let mut rate = kbps.unwrap_or(2000) * 1000 / 8; // bytes a second
        let (mut frame, mut bytes_sent, mut dropped) = (0u32, 0u64, 0u64);
        let mut rates = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_micros(1_000_000 / FPS));
        let until = Instant::now() + Duration::from_secs(seconds);
        let mut adjusted = Instant::now();
        while Instant::now() < until {
            tick.tick().await;
            if kbps.is_none() && adjusted.elapsed() >= Duration::from_millis(100) {
                let s = self.conn.stats().path;
                let shared = self.rate.load(Ordering::Relaxed);
                // The controller's own rate where it says one (gcc), less some
                // for the keyframes and the rest; else the window's.
                let estimate = if shared > 0 { shared as f64 * 0.85 } else { s.cwnd as f64 / s.rtt.as_secs_f64().max(0.001) * 0.8 };
                rate = (estimate as u64).clamp(30_000, 2_500_000); // 20 Mbit/s at most, the desktop's
                rates.push(rate);
                adjusted = Instant::now();
            }
            let average = rate / FPS;
            let size = if frame as u64 % KEYFRAME_EVERY == 0 { average * KEYFRAME_SIZE } else { average } as usize;
            let count = size.div_ceil(payload).max(1);
            sent_at.lock().unwrap().insert(frame, now_us(self.start));
            for index in 0..count {
                let mut d = Vec::with_capacity(FRAME_HEADER + payload);
                d.push(FRAME);
                d.extend_from_slice(&frame.to_be_bytes());
                d.extend_from_slice(&(index as u16).to_be_bytes());
                d.extend_from_slice(&(count as u16).to_be_bytes());
                d.resize(FRAME_HEADER + size.saturating_sub(index * payload).min(payload).max(1), 0);
                bytes_sent += d.len() as u64;
                if !send_realtime(&self.conn, &self.rate, Bytes::from(d)) {
                    dropped += 1;
                }
            }
            frame += 1;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
        *self.acks.lock().unwrap() = None;
        collector.abort();
        let mut l = latencies.lock().unwrap().clone();
        l.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let whole = acked.load(Ordering::Relaxed);
        let average_rate = if rates.is_empty() { rate } else { rates.iter().sum::<u64>() / rates.len() as u64 };
        format!("{whole}/{frame} frames whole ({:.1}%), {:.0} kbit/s sent (target avg {} kbit/s), frame to ack p50 {:.1} ms, p90 {:.1} ms, p99 {:.1} ms, {dropped} not sent",
                100.0 * whole as f64 / frame.max(1) as f64, bytes_sent as f64 * 8.0 / 1000.0 / seconds as f64,
                average_rate * 8 / 1000, percentile(&l, 0.5), percentile(&l, 0.9), percentile(&l, 0.99))
    }
}

async fn rt_report(client: &Client, seconds: u64) -> String {
    let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
    *client.echoes.lock().unwrap() = Some(tx);
    let start = client.start;
    let rtts: Arc<Mutex<Vec<f64>>> = Arc::default();
    let collector = {
        let rtts = rtts.clone();
        tokio::spawn(async move {
            while let Some(t) = rx.recv().await {
                rtts.lock().unwrap().push((now_us(start) - t) as f64 / 1000.0);
            }
        })
    };
    let until = Instant::now() + Duration::from_secs(seconds);
    let mut sent = 0u64;
    let mut tick = tokio::time::interval(Duration::from_millis(5));
    while Instant::now() < until {
        tick.tick().await;
        let mut d = vec![ECHO];
        d.extend_from_slice(&now_us(client.start).to_be_bytes());
        d.resize(1009, 0);
        if send_realtime(&client.conn, &client.rate, Bytes::from(d)) {
            sent += 1;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    *client.echoes.lock().unwrap() = None;
    collector.abort();
    let mut r = rtts.lock().unwrap().clone();
    r.sort_by(|a, b| a.partial_cmp(b).unwrap());
    format!("{}/{} back, p50 {:.1} ms, p99 {:.1} ms", r.len(), sent, percentile(&r, 0.5), percentile(&r, 0.99))
}

async fn client(address: SocketAddr, cc: String, seconds: u64, tests: Vec<String>) {
    let c = Client::connect(address, &cc).await;
    for test in tests {
        let line = match test.as_str() {
            "rt" => rt_report(&c, seconds).await,
            "up" => format!("{:.2} MB/s", c.up(seconds).await),
            "down" => format!("{:.2} MB/s", c.down(seconds).await),
            "both" => {
                let (u, d) = tokio::join!(c.up(seconds), c.down(seconds));
                format!("up {u:.2} MB/s, down {d:.2} MB/s")
            }
            "rt+both" => {
                let (r, u, d) = tokio::join!(rt_report(&c, seconds), c.up(seconds), c.down(seconds));
                format!("{r}; up {u:.2} MB/s, down {d:.2} MB/s")
            }
            "rt+up" => {
                let (r, u) = tokio::join!(rt_report(&c, seconds), c.up(seconds));
                format!("{r}; up {u:.2} MB/s")
            }
            "rt+down" => {
                let (r, d) = tokio::join!(rt_report(&c, seconds), c.down(seconds));
                format!("{r}; down {d:.2} MB/s")
            }
            "video:auto+up" => {
                let (v, u) = tokio::join!(c.video(seconds, None), c.up(seconds));
                format!("{v}; up {u:.2} MB/s")
            }
            "video:auto" => c.video(seconds, None).await,
            t if t.starts_with("video:") => c.video(seconds, t[6..].parse().ok()).await,
            other => format!("unknown test {other}"),
        };
        println!("{cc} {test}: {line}");
        println!("    PC:     {}", path_stats(&c.conn));
        println!("    tablet: {}", c.their_stats().await);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    c.conn.close(0u32.into(), b"done");
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("probe") => {
            // What quinn-udp makes of this system's UDP: GSO, GRO, DF.
            let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            let state = quinn_udp::UdpSocketState::new((&socket).into()).unwrap();
            println!("gso segments {}, gro segments {}, may fragment {}", state.max_gso_segments(),
                     state.gro_segments(), state.may_fragment());
        }
        Some("server") => server(args[2].parse().unwrap(), args.get(3).cloned().unwrap_or("cubic".into())).await,
        Some("client") => {
            let address = tokio::net::lookup_host(&args[2]).await.unwrap().next().unwrap();
            client(address, args[3].clone(), args[4].parse().unwrap(), args[5..].to_vec()).await
        }
        _ => eprintln!("qbench server PORT CC | qbench client HOST:PORT CC SECONDS TEST..."),
    }
}
