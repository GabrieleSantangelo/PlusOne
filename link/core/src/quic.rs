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
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TransportConfig};
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
/// What quinn takes from the session's stream before it is sent: about a
/// fast path's bandwidth-delay product, no more (it waits behind nothing
/// realtime, which goes as datagrams, but a path's change requeues it).
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

/// The core's own runtime for quinn: two threads, whatever the program.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
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

fn server_config(server: &Arc<rustls::ServerConfig>) -> Result<(ServerConfig, Arc<AtomicU64>), String> {
    let mut tls = (**server).clone();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|e| e.to_string())?;
    let rate: Arc<AtomicU64> = Arc::default();
    let mut config = ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport(rate.clone())));
    Ok((config, rate))
}

/// An endpoint bound to address (port 0: any): it dials, and with server
/// configs it is dialed too.
pub(crate) fn endpoint(address: SocketAddr, server: Option<&Arc<rustls::ServerConfig>>) -> io::Result<Endpoint> {
    let server = match server {
        Some(server) => Some(server_config(server).map_err(io::Error::other)?.0),
        None => None,
    };
    let socket = std::net::UdpSocket::bind(address)?;
    let _entered = runtime().enter();
    Endpoint::new(EndpointConfig::default(), server, socket, Arc::new(quinn::TokioRuntime))
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
/// its own: the session's TLS and hellos block). handshakes: those not yet
/// a path, counted with the TCP ones; at most `limit` at once.
pub(crate) fn listen(endpoint: Endpoint, server: Arc<rustls::ServerConfig>, handshakes: Arc<std::sync::atomic::AtomicUsize>,
                     limit: usize, whole: Duration, on: impl Fn(UnixStream, Quic) + Send + Sync + 'static) {
    let on = Arc::new(on);
    runtime().spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            if handshakes.fetch_add(1, Ordering::SeqCst) >= limit {
                handshakes.fetch_sub(1, Ordering::SeqCst);
                incoming.refuse();
                continue;
            }
            let Ok((config, rate)) = server_config(&server) else {
                handshakes.fetch_sub(1, Ordering::SeqCst);
                continue;
            };
            let (on, handshakes) = (on.clone(), handshakes.clone());
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
                            handshakes.fetch_sub(1, Ordering::SeqCst);
                        });
                        if spawned.is_err() {
                            log!("{from}: QUIC: no thread for it");
                        }
                    }
                    Ok(Err(e)) => {
                        handshakes.fetch_sub(1, Ordering::SeqCst);
                        log!("{from}: QUIC: {e}");
                    }
                    Err(_) => {
                        handshakes.fetch_sub(1, Ordering::SeqCst);
                        log!("{from}: QUIC: no stream in time");
                    }
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
