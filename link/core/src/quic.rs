//! QUIC for the IP paths (architecture §3.14; chosen after the Stage 1
//! spike, link/quic-spike). A QUIC connection is a path of the session as a
//! TCP connection is: its first bidirectional stream carries the session's
//! frames, bridged to a socket pair as the cable's are (little room on the
//! session's end), and its datagrams the session's sealed realtime ones.
//! The congestion controller is ours (cc.rs): the stream's bulk and the
//! datagrams share one rate, the datagrams first in each packet (quinn's
//! order), and realtime that would wait longer than LATE for the window is
//! dropped, not queued. The identities are the TCP path's (the same rustls
//! configs, pinned both ways). For now the session's own TLS still runs
//! inside the stream (sealed twice); a later step leaves it to QUIC's.

use crate::log;
use bytes::Bytes;
use quinn::{ClientConfig, Connection, ConnectionId, ConnectionIdGenerator, Endpoint, EndpointConfig, ServerConfig,
            TransportConfig};
use crate::gate::Gate;
use std::io;
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// ALPN: no other QUIC service taken for ours, nor ours for one.
const ALPN: &[u8] = b"plus-one-link/1";
/// The MTU from the first packet: a local network's 1500 less IP and UDP
/// (quinn lowers it where packets that big are lost).
const LOCAL_MTU: u16 = 1452;
/// Realtime that would wait longer than this for the window: dropped.
const LATE: Duration = Duration::from_millis(100);
const DATAGRAM_SEND_BUFFER: usize = 512 << 10;
/// What quinn takes from the session's stream before it is acknowledged,
/// at first (about a fast path's bandwidth-delay product); then the
/// bridge has it follow the rate (WINDOW_EVERY).
const SEND_WINDOW: u64 = 2 << 20;
const BRIDGE_CHUNK: usize = 16 << 10;
/// What the bridge hands quinn ahead of the controller's rate: this long's
/// worth (at least AHEAD_LEAST). quinn keeps what it is handed in order,
/// whatever comes after: handed all at once (up to SEND_WINDOW), a control
/// message waited behind seconds of a transfer on a slow path, and an
/// opened channel was never answered. As TCP_NOTSENT_LOWAT on a TCP path.
const AHEAD: Duration = Duration::from_millis(30);
const AHEAD_LEAST: f64 = 32.0 * 1024.0;
/// How often quinn's send window follows the rate (Bridge): what it holds
/// unacknowledged, sent or not, the rate's worth of a round trip and AHEAD
/// (handed a quarter over the rate, it kept up to SEND_WINDOW unsent: a
/// second on a 15 Mbit/s path, and a channel's open waited behind it).
const WINDOW_EVERY: Duration = Duration::from_millis(10);
/// A connection ID: this many random bytes, then this many of their MAC
/// (MacCids).
const CID_NONCE: usize = 8;
const CID_MAC: usize = 4;
/// Over this many handshakes at once (gate.rs), an address proves it is
/// one (a Retry's token back) before its handshake costs anything, or
/// counts as its (architecture §3.14: under load, Retry is the cookie).
const RETRY_OVER: usize = 2;

/// The core's own runtime for quinn: two threads, whatever the program.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("link-quic").enable_all().build()
            .expect("a runtime for QUIC")
    })
}

fn transport(rate: Arc<AtomicU64>) -> TransportConfig {
    let mut t = TransportConfig::default();
    // The session says when a path is dead (PATH_SILENCE); QUIC's own
    // timeout only frees what the session dropped.
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("a timeout")));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.initial_mtu(LOCAL_MTU);
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((16u32 << 20).into());
    t.send_window(SEND_WINDOW);
    t.datagram_receive_buffer_size(Some(4 << 20));
    t.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    // One stream, the dialer's: the session's (architecture §3.14, SEC-19).
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    // The peer acknowledges every second packet within 2 ms: cc.rs reads
    // the delay's growth off the acknowledgements' times.
    let mut ack = quinn::AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(1u32.into()).max_ack_delay(Some(Duration::from_millis(2)));
    t.ack_frequency_config(Some(ack));
    t.congestion_controller_factory(Arc::new(crate::cc::GccConfig { rate }));
    t
}

/// A QUIC path's connection, and the rate its controller allows (bytes a
/// second; 0 before the first acknowledgement).
#[derive(Clone)]
pub struct Quic {
    pub(crate) conn: Connection,
    rate: Arc<AtomicU64>,
    /// Realtime bytes sent on it, all told: the bulk gets what they leave.
    realtime: Arc<AtomicU64>,
}

impl Quic {
    /// A sealed datagram out, unless it would wait longer than LATE for the
    /// window or does not fit: false if dropped.
    pub(crate) fn send(&self, sealed: &[u8]) -> bool {
        let rate = self.rate.load(Ordering::Relaxed);
        let waiting = DATAGRAM_SEND_BUFFER.saturating_sub(self.conn.datagram_send_buffer_space());
        if rate > 0 && waiting as f64 > rate as f64 * LATE.as_secs_f64() {
            return false;
        }
        let sent = self.conn.send_datagram(Bytes::copy_from_slice(sealed)).is_ok();
        if sent {
            self.realtime.fetch_add(sealed.len() as u64, Ordering::Relaxed);
        }
        sent
    }

    /// What the controller allows now (bytes a second).
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.conn.close(0u32.into(), b"path over");
    }

    /// Each datagram that comes, to on (on the runtime: it must not block).
    pub(crate) fn read_datagrams(&self, on: impl Fn(Bytes) + Send + 'static) {
        let conn = self.conn.clone();
        runtime().spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                on(datagram);
            }
        });
    }
}

fn client_config(client: &Arc<rustls::ClientConfig>) -> Result<(ClientConfig, Arc<AtomicU64>), String> {
    let mut tls = (**client).clone();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let rate: Arc<AtomicU64> = Arc::default();
    let mut config = ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport(rate.clone())));
    Ok((config, rate))
}

/// The server's TLS for QUIC: made once, for every connection.
fn server_crypto(server: &Arc<rustls::ServerConfig>) -> Result<Arc<quinn::crypto::rustls::QuicServerConfig>, String> {
    let mut tls = (**server).clone();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|e| e.to_string())?))
}

/// A connection's: its own controller, whose rate it shares.
fn server_config(crypto: &Arc<quinn::crypto::rustls::QuicServerConfig>) -> (ServerConfig, Arc<AtomicU64>) {
    let rate: Arc<AtomicU64> = Arc::default();
    let mut config = ServerConfig::with_crypto(crypto.clone());
    config.transport_config(Arc::new(transport(rate.clone())));
    (config, rate)
}

/// An endpoint bound to address (port 0: any): it dials, and with server
/// configs it is dialed too.
pub(crate) fn endpoint(address: SocketAddr, server: Option<&Arc<rustls::ServerConfig>>) -> io::Result<Endpoint> {
    let server = match server {
        Some(server) => Some(server_config(&server_crypto(server).map_err(io::Error::other)?).0),
        None => None,
    };
    let socket = std::net::UdpSocket::bind(address)?;
    let key = ring::hmac::Key::generate(ring::hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
        .map_err(|_| io::Error::other("no random key"))?;
    let mut config = EndpointConfig::default();
    config.cid_generator(move || Box::new(MacCids(key.clone())));
    let _entered = runtime().enter();
    Endpoint::new(config, server, socket, Arc::new(quinn::TokioRuntime))
}

/// Connection IDs that carry a MAC under a key of the endpoint's
/// (architecture §3.14, F-2). quinn checks it on a packet for no
/// connection it knows before anything else, and drops it if it fails: a
/// stranger's packets get nothing back, no stateless reset either.
struct MacCids(ring::hmac::Key);

impl MacCids {
    fn mac(&self, nonce: &[u8]) -> [u8; CID_MAC] {
        let tag = ring::hmac::sign(&self.0, nonce);
        let mut mac = [0u8; CID_MAC];
        mac.copy_from_slice(&tag.as_ref()[..CID_MAC]);
        mac
    }
}

impl ConnectionIdGenerator for MacCids {
    fn generate_cid(&mut self) -> ConnectionId {
        let mut cid = [0u8; CID_NONCE + CID_MAC];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut cid[..CID_NONCE])
            .expect("random bytes");
        let mac = self.mac(&cid[..CID_NONCE]);
        cid[CID_NONCE..].copy_from_slice(&mac);
        ConnectionId::new(&cid)
    }

    fn validate(&self, cid: &ConnectionId) -> Result<(), quinn_proto::InvalidCid> {
        if cid.len() != CID_NONCE + CID_MAC {
            return Err(quinn_proto::InvalidCid);
        }
        let mac = self.mac(&cid[..CID_NONCE]);
        // In constant time.
        let differ = mac.iter().zip(&cid[CID_NONCE..]).fold(0u8, |d, (a, b)| d | (a ^ b));
        if differ == 0 { Ok(()) } else { Err(quinn_proto::InvalidCid) }
    }

    fn cid_len(&self) -> usize {
        CID_NONCE + CID_MAC
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

/// A path to the device at address: the session's stream (bridged) and the
/// connection. Blocking (not on the runtime).
pub(crate) fn dial(endpoint: &Endpoint, client: &Arc<rustls::ClientConfig>, address: SocketAddr, timeout: Duration)
                   -> Result<(UnixStream, Quic), String> {
    let (config, rate) = client_config(client)?;
    runtime().block_on(async {
        let connecting = endpoint.connect_with(config, address, "plus-one.invalid").map_err(|e| e.to_string())?;
        let conn = tokio::time::timeout(timeout, connecting).await.map_err(|_| "no answer".to_owned())?
            .map_err(|e| e.to_string())?;
        // The other side sees the stream once something goes on it: the
        // session's TLS hello, which the dialer writes first.
        let (send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let realtime: Arc<AtomicU64> = Arc::default();
        let stream = bridge(&conn, send, recv, rate.clone(), realtime.clone()).map_err(|e| e.to_string())?;
        Ok((stream, Quic { conn, rate, realtime }))
    })
}

/// Paired devices' QUIC connections, each a path: to on (on a thread of
/// its own: the session's TLS and hellos block). The gate says who may
/// start a handshake; the others are ignored (nothing back: a flood
/// learns nothing).
pub(crate) fn listen(endpoint: Endpoint, server: Arc<rustls::ServerConfig>, gate: Arc<Gate>, whole: Duration,
                     on: impl Fn(UnixStream, Quic) + Send + Sync + 'static) {
    let on = Arc::new(on);
    let crypto = match server_crypto(&server) {
        Ok(crypto) => crypto,
        Err(e) => return log!("QUIC: {e}"),
    };
    runtime().spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            if gate.running() >= RETRY_OVER && !incoming.remote_address_validated() && incoming.may_retry() {
                let _ = incoming.retry();
                continue;
            }
            let Some(handshake) = gate.start(incoming.remote_address().ip()) else {
                incoming.ignore();
                continue;
            };
            let (config, rate) = server_config(&crypto);
            let on = on.clone();
            tokio::spawn(async move {
                let from = incoming.remote_address();
                let accepted = async {
                    let conn = incoming.accept_with(Arc::new(config)).map_err(|e| e.to_string())?.await
                        .map_err(|e| e.to_string())?;
                    let (send, recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
                    let realtime: Arc<AtomicU64> = Arc::default();
                    let stream = bridge(&conn, send, recv, rate.clone(), realtime.clone()).map_err(|e| e.to_string())?;
                    Ok::<_, String>((stream, Quic { conn, rate, realtime }))
                };
                match tokio::time::timeout(whole, accepted).await {
                    Ok(Ok((stream, quic))) => {
                        // The session's TLS and hellos block: a thread, which
                        // gives the handshake back when it is done.
                        let spawned = std::thread::Builder::new().name("link-accept".into()).spawn(move || {
                            on(stream, quic);
                            drop(handshake);
                        });
                        if spawned.is_err() {
                            log!("{from}: QUIC: no thread for it");
                        }
                    }
                    Ok(Err(e)) => log!("{from}: QUIC: {e}"),
                    Err(_) => log!("{from}: QUIC: no stream in time"),
                }
            });
        }
    });
}

/// A QUIC stream bridged to a socket pair: the session runs on the pair's
/// end this returns, as on a TCP connection. Either side over ends both.
fn bridge(conn: &Connection, mut send: quinn::SendStream, mut recv: quinn::RecvStream, rate: Arc<AtomicU64>,
          realtime: Arc<AtomicU64>) -> io::Result<UnixStream> {
    let conn = conn.clone();
    let (ours, theirs) = UnixStream::pair()?;
    crate::stream::little_room(&theirs);
    ours.set_nonblocking(true)?;
    let ours = {
        let _entered = runtime().enter();
        tokio::net::UnixStream::from_std(ours)?
    };
    let (mut from_session, mut to_session) = ours.into_split();
    runtime().spawn(async move {
        let mut buf = vec![0u8; BRIDGE_CHUNK];
        while let Ok(Some(n)) = recv.read(&mut buf).await {
            if to_session.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = to_session.shutdown().await;
    });
    runtime().spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; BRIDGE_CHUNK];
        // A token bucket at what the controller's rate leaves the realtime
        // (a quarter over the rate, for it to see the window full and grow;
        // the realtime's own rate a fifth over, smoothed), AHEAD of it deep:
        // the bulk on what is left (§3.11), the realtime ahead.
        let (mut tokens, mut refilled) = (AHEAD_LEAST, tokio::time::Instant::now());
        let (mut realtime_rate, mut realtime_seen) = (0.0f64, realtime.load(Ordering::Relaxed));
        let mut windowed = refilled;
        loop {
            match from_session.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    loop {
                        let now = tokio::time::Instant::now();
                        if now.duration_since(windowed) >= WINDOW_EVERY {
                            let held = rate.load(Ordering::Relaxed) as f64 * (conn.rtt() + AHEAD).as_secs_f64();
                            conn.set_send_window((held as u64).max(2 * BRIDGE_CHUNK as u64));
                            windowed = now;
                        }
                        let elapsed = now.duration_since(refilled).as_secs_f64();
                        let sent = realtime.load(Ordering::Relaxed);
                        if elapsed > 0.0 {
                            let weight = (elapsed / 0.2).min(1.0);
                            realtime_rate += weight * ((sent - realtime_seen) as f64 / elapsed - realtime_rate);
                            realtime_seen = sent;
                        }
                        let total = (rate.load(Ordering::Relaxed) as f64).max(AHEAD_LEAST) * 1.25;
                        let rate = (total - 1.2 * realtime_rate).max(AHEAD_LEAST / 2.0);
                        let depth = (rate * AHEAD.as_secs_f64()).max(AHEAD_LEAST);
                        tokens = (tokens + rate * elapsed).min(depth);
                        refilled = now;
                        if tokens >= n as f64 || tokens >= depth {
                            break;
                        }
                        let wait = (n as f64 - tokens) / rate;
                        tokio::time::sleep(Duration::from_secs_f64(wait.min(0.01))).await;
                    }
                    tokens -= n as f64;
                    if send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = send.finish();
    });
    Ok(theirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, Paired};
    use std::io::{Read, Write};
    use std::sync::Mutex;
    use std::time::Instant;

    fn identity() -> Identity {
        let made = rcgen::generate_simple_self_signed(vec!["plus-one".into()]).unwrap();
        Identity::new(made.cert.der().to_vec(), made.key_pair.serialize_der()).unwrap()
    }

    fn only(fingerprint: String) -> Paired {
        Arc::new(move |seen: &str| seen == fingerprint)
    }

    /// A tablet's QUIC listener for pc: its address, and the session's end
    /// of each path that comes.
    fn tablet_for(pc: &Identity) -> (Identity, SocketAddr, std::sync::mpsc::Receiver<(UnixStream, Quic)>) {
        let tablet = identity();
        let server = tablet.server_config(only(pc.fingerprint())).unwrap();
        let endpoint = endpoint(([127, 0, 0, 1], 0).into(), Some(&server)).unwrap();
        let address = endpoint.local_addr().unwrap();
        let (paths, came) = std::sync::mpsc::channel();
        let paths = Mutex::new(paths);
        listen(endpoint, server, Gate::new(), Duration::from_secs(5), move |stream, quic| {
            let _ = paths.lock().unwrap().send((stream, quic));
        });
        (tablet, address, came)
    }

    #[test]
    fn connection_ids_carry_their_mac() {
        let key = |seed: u8| ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[seed; 32]);
        let mut ours = MacCids(key(1));
        let cid = ours.generate_cid();
        assert_eq!(cid.len(), CID_NONCE + CID_MAC);
        assert!(ours.validate(&cid).is_ok());
        assert!(MacCids(key(2)).validate(&cid).is_err());
        let mut forged = cid.to_vec();
        forged[0] ^= 1;
        assert!(ours.validate(&ConnectionId::new(&forged)).is_err());
        assert!(ours.validate(&ConnectionId::new(&forged[..8])).is_err());
    }

    #[test]
    fn a_strangers_packets_get_nothing_back() {
        let (_tablet, address, _paths) = tablet_for(&identity());
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let random = ring::rand::SystemRandom::new();
        for i in 0..64u8 {
            let mut packet = vec![0u8; 40 + i as usize * 16];
            ring::rand::SecureRandom::fill(&random, &mut packet).unwrap();
            // Short headers (a connection's packets, with IDs of no one's),
            // and long ones that are no first packet.
            if i % 2 == 0 {
                packet[0] = 0x40 | (packet[0] & 0x3f);
            } else {
                packet[0] = 0xe0 | (packet[0] & 0x0f);  // QUIC v1's Handshake
                packet[1..5].copy_from_slice(&1u32.to_be_bytes());
                packet[5] = (CID_NONCE + CID_MAC) as u8;
            }
            socket.send_to(&packet, address).unwrap();
        }
        let mut buf = [0u8; 2048];
        assert!(socket.recv_from(&mut buf).is_err(), "an answer to a stranger");
    }

    #[test]
    fn a_flood_of_handshakes_holds_up_no_one_else() {
        let pc = identity();
        let (tablet, address, paths) = tablet_for(&pc);
        // Strangers (their certificates no paired one's) from one address,
        // all at once; the PC meanwhile from another.
        let strangers: Vec<_> = (0..32).map(|_| {
            let client = identity().client_config(only(tablet.fingerprint())).unwrap();
            std::thread::spawn(move || {
                let endpoint = endpoint(([127, 0, 0, 1], 0).into(), None).unwrap();
                let _ = dial(&endpoint, &client, address, Duration::from_secs(3));
            })
        }).collect();
        std::thread::sleep(Duration::from_millis(50));
        let client = pc.client_config(only(tablet.fingerprint())).unwrap();
        let endpoint = endpoint(([127, 0, 0, 2], 0).into(), None).unwrap();
        let started = Instant::now();
        let (mut stream, _quic) = dial(&endpoint, &client, address, Duration::from_secs(3)).unwrap();
        stream.write_all(b"hello").unwrap();
        let (mut theirs, _) = paths.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut hello = [0u8; 5];
        theirs.read_exact(&mut hello).unwrap();
        assert_eq!(&hello, b"hello");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        for stranger in strangers {
            stranger.join().unwrap();
        }
        assert!(paths.try_recv().is_err(), "a stranger's path");
    }

    #[test]
    fn keys_change_and_the_stream_goes_on() {
        let pc = identity();
        let (tablet, address, paths) = tablet_for(&pc);
        let client = pc.client_config(only(tablet.fingerprint())).unwrap();
        let endpoint = endpoint(([127, 0, 0, 1], 0).into(), None).unwrap();
        let (mut stream, quic) = dial(&endpoint, &client, address, Duration::from_secs(3)).unwrap();
        // 2 MiB: over 1,000 packets, so quinn changes keys on its own too
        // (the first time within 10 to 1,000; then before the AEAD's limit).
        let sent: Vec<u8> = (0..2u32 << 20).map(|i| (i % 251) as u8).collect();
        let writer = {
            let sent = sent.clone();
            std::thread::spawn(move || {
                for (i, chunk) in sent.chunks(256 << 10).enumerate() {
                    if i % 2 == 1 {
                        quic.conn.force_key_update();
                    }
                    stream.write_all(chunk).unwrap();
                }
                stream
            })
        };
        let (mut theirs, _) = paths.recv_timeout(Duration::from_secs(3)).unwrap();
        theirs.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut came = vec![0u8; sent.len()];
        theirs.read_exact(&mut came).unwrap();
        assert!(came == sent);
        drop(writer.join().unwrap());
    }
}
