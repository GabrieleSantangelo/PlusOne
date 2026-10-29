//! QUIC for the IP paths (architecture §3.14; chosen after the Stage 1
//! spike, link/quic-spike). A QUIC connection is a path of the session as a
//! TCP connection is: its first bidirectional stream carries the session's
//! frames, bridged to a socket pair as the cable's are (little room on the
//! session's end), and its datagrams the session's sealed realtime ones.
//! The congestion controller is ours (cc.rs): the stream's bulk and the
//! datagrams share one rate, the datagrams first in each packet (quinn's
//! order), and realtime that would wait longer than LATE for the window is
//! dropped, not queued. A Video channel's groups (a video's frames) go on
//! unidirectional streams, one each, ahead of the session's stream and
//! behind the datagrams (groups.rs). The identities are the TCP path's (the same rustls
//! configs, pinned both ways). For now the session's own TLS still runs
//! inside the stream (sealed twice); a later step leaves it to QUIC's.

use crate::log;
use bytes::Bytes;
use quinn::{ClientConfig, Connection, ConnectionId, ConnectionIdGenerator, Endpoint, EndpointConfig, ServerConfig,
            TransportConfig};
use crate::cc::{Budget, GccConfig};
use crate::gate::Gate;
use crate::groups::{self, Groups};
use std::io;
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// ALPN: no other QUIC service taken for ours, nor ours for one.
const ALPN: &[u8] = b"plus-one-link/1";
/// The MTU from the first packet on, and never less: a local network's
/// 1500 less IPv6 and UDP. quinn's black hole detection took a full
/// queue's lost tails for a smaller path and went down to 1200 for a
/// minute, where no realtime datagram of MAX_DATAGRAM fitted: all dropped,
/// the video's whole frames 0 % on an emulated ADSL. A path that does not
/// carry it fails the handshake (its Initials are this big), and the
/// session stays on TCP.
const LOCAL_MTU: u16 = 1452;
/// Realtime that would wait longer than this for the window: dropped (a
/// group older than this: reset, groups.rs).
pub(crate) const LATE: Duration = Duration::from_millis(100);
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

fn transport(cc: Arc<GccConfig>) -> TransportConfig {
    let mut t = TransportConfig::default();
    // The session says when a path is dead (PATH_SILENCE); QUIC's own
    // timeout only frees what the session dropped.
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("a timeout")));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.initial_mtu(LOCAL_MTU);
    t.min_mtu(LOCAL_MTU);
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((16u32 << 20).into());
    t.send_window(SEND_WINDOW);
    t.datagram_receive_buffer_size(Some(4 << 20));
    t.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    // One bidirectional stream, the dialer's: the session's (architecture
    // §3.14, SEC-19); unidirectional ones, Video's groups (groups.rs),
    // MAX_STREAMS at once, what comes on them sealed as the datagrams are.
    // Their data is bounded by the receive windows (the connection's, 16
    // MiB; quinn has one window for every stream, the session's 8 MiB).
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(groups::MAX_STREAMS.into());
    // The peer acknowledges every second packet within 2 ms: cc.rs reads
    // the delay's growth off the acknowledgements' times.
    let mut ack = quinn::AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(1u32.into()).max_ack_delay(Some(Duration::from_millis(2)));
    t.ack_frequency_config(Some(ack));
    t.congestion_controller_factory(cc);
    t
}

/// A QUIC path's connection, and the rate its controller allows (bytes a
/// second; 0 before the first acknowledgement).
#[derive(Clone)]
pub struct Quic {
    pub(crate) conn: Connection,
    /// What its controller shares: the rate, the budget told.
    cc: Arc<GccConfig>,
    /// Realtime bytes sent on it, all told: the bulk gets what they leave.
    realtime: Arc<AtomicU64>,
    /// quinn's datagram room with none waiting: what it holds is this less
    /// the room now. Not DATAGRAM_SEND_BUFFER: quinn-proto 0.11.17 keeps 32
    /// bytes aside, and 32 a datagram waiting; empty, it read 32 bytes still
    /// to send, and the desktop's wait for none never ended.
    idle: usize,
    /// Video's groups on streams (groups.rs), if the other side reads them
    /// (its hello: set_groups).
    groups: Arc<Groups>,
    peer_groups: bool,
}

/// quinn's send window, all streams': the rate's worth of a round trip and
/// AHEAD (the bulk ahead of the rate: bridge), and LATE's worth more while
/// groups go (a group waits for room no longer than that: groups.rs).
pub(crate) fn send_window(rate: u64, rtt: Duration, groups: bool) -> u64 {
    let ahead = rtt + AHEAD + if groups { LATE } else { Duration::ZERO };
    ((rate as f64 * ahead.as_secs_f64()) as u64).max(2 * BRIDGE_CHUNK as u64)
}

impl Quic {
    /// A sealed datagram out, unless it would wait longer than LATE for the
    /// window or does not fit: false if dropped.
    pub(crate) fn send(&self, sealed: &[u8]) -> bool {
        let rate = self.cc.rate.load(Ordering::Relaxed);
        let room = self.conn.datagram_send_buffer_space();
        // Nor past quinn's room: it would drop the oldest to make some,
        // unseen (over about 42 Mbit/s, LATE's worth is more than it holds).
        if rate > 0 && self.idle.saturating_sub(room) as f64 > rate as f64 * LATE.as_secs_f64() || sealed.len() > room {
            return false;
        }
        let sent = self.conn.send_datagram(Bytes::copy_from_slice(sealed)).is_ok();
        if sent {
            self.realtime.fetch_add(sealed.len() as u64, Ordering::Relaxed);
        }
        sent
    }

    /// The datagrams handed to quinn and not sent yet, and the groups'
    /// messages not yet on their streams (bytes).
    pub(crate) fn queued(&self) -> usize {
        self.idle.saturating_sub(self.conn.datagram_send_buffer_space()) + self.groups.queued()
    }

    /// The other side reads groups on streams (its hello says so).
    pub(crate) fn set_groups(&mut self, on: bool) {
        self.peer_groups = on;
    }

    pub(crate) fn groups(&self) -> bool {
        self.peer_groups
    }

    /// A sealed message of channel's group (key: a key group; last: the
    /// group's last) to the group's stream (groups.rs): false if dropped.
    pub(crate) fn send_in_group(&self, channel: u16, group: u32, key: bool, last: bool, sealed: &[u8]) -> bool {
        self.groups.send(channel, group, key, last, sealed, |pieces| {
            let writing = groups::write(self.conn.clone(), pieces, self.groups.shared.clone(), self.cc.rate.clone(),
                                        self.realtime.clone());
            runtime().spawn(writing);
        })
    }

    /// The groups so far, and the controller's losses and cuts.
    pub(crate) fn group_stats(&self) -> groups::Stats {
        let mut stats = self.groups.shared.stats();
        stats.datagram_bytes = self.realtime.load(Ordering::Relaxed).saturating_sub(stats.stream_bytes);
        stats.lost_bytes = self.cc.losses.lost.load(Ordering::Relaxed);
        stats.lost_after_cut = self.cc.losses.after_cut.load(Ordering::Relaxed);
        stats.cuts = self.cc.losses.cuts.load(Ordering::Relaxed);
        stats
    }

    /// What the controller allows now (bytes a second).
    pub fn rate(&self) -> u64 {
        self.cc.rate.load(Ordering::Relaxed)
    }

    /// The budget its controller tells, as it moves (cc.rs Budget).
    pub(crate) fn budget(&self) -> Arc<Budget> {
        self.cc.budget.clone()
    }

    pub(crate) fn close(&self) {
        self.cc.budget.close();
        let stats = self.group_stats();
        if stats.groups > 0 {
            log!("{}: QUIC path over: {stats}", self.conn.remote_address());
        }
        self.conn.close(0u32.into(), b"path over");
    }

    /// Each datagram that comes, and each message of the other side's
    /// groups (groups.rs), to on (on the runtime: it must not block).
    pub(crate) fn read_datagrams(&self, on: impl Fn(Bytes) + Send + Sync + 'static) {
        let on = Arc::new(on);
        let conn = self.conn.clone();
        let datagrams = on.clone();
        runtime().spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                datagrams(datagram);
            }
        });
        runtime().spawn(groups::read(self.conn.clone(), on));
    }
}

fn client_config(client: &Arc<rustls::ClientConfig>) -> Result<(ClientConfig, Arc<GccConfig>), String> {
    let mut tls = (**client).clone();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let cc: Arc<GccConfig> = Arc::default();
    let mut config = ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport(cc.clone())));
    Ok((config, cc))
}

/// The server's TLS for QUIC: made once, for every connection.
fn server_crypto(server: &Arc<rustls::ServerConfig>) -> Result<Arc<quinn::crypto::rustls::QuicServerConfig>, String> {
    let mut tls = (**server).clone();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|e| e.to_string())?))
}

/// A connection's: its own controller, and what it shares.
fn server_config(crypto: &Arc<quinn::crypto::rustls::QuicServerConfig>) -> (ServerConfig, Arc<GccConfig>) {
    let cc: Arc<GccConfig> = Arc::default();
    let mut config = ServerConfig::with_crypto(crypto.clone());
    config.transport_config(Arc::new(transport(cc.clone())));
    (config, cc)
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
    let (config, cc) = client_config(client)?;
    runtime().block_on(async {
        let connecting = endpoint.connect_with(config, address, "plus-one.invalid").map_err(|e| e.to_string())?;
        let conn = tokio::time::timeout(timeout, connecting).await.map_err(|_| "no answer".to_owned())?
            .map_err(|e| e.to_string())?;
        // The other side sees the stream once something goes on it: the
        // session's TLS hello, which the dialer writes first.
        let (send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let realtime: Arc<AtomicU64> = Arc::default();
        let groups: Arc<Groups> = Arc::default();
        let stream = bridge(&conn, send, recv, cc.rate.clone(), realtime.clone(), groups.shared.clone())
            .map_err(|e| e.to_string())?;
        let idle = conn.datagram_send_buffer_space();
        Ok((stream, Quic { conn, cc, realtime, idle, groups, peer_groups: false }))
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
            let (config, cc) = server_config(&crypto);
            let on = on.clone();
            tokio::spawn(async move {
                let from = incoming.remote_address();
                let accepted = async {
                    let conn = incoming.accept_with(Arc::new(config)).map_err(|e| e.to_string())?.await
                        .map_err(|e| e.to_string())?;
                    let (send, recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
                    let realtime: Arc<AtomicU64> = Arc::default();
                    let groups: Arc<Groups> = Arc::default();
                    let stream = bridge(&conn, send, recv, cc.rate.clone(), realtime.clone(), groups.shared.clone())
                        .map_err(|e| e.to_string())?;
                    let idle = conn.datagram_send_buffer_space();
                    Ok::<_, String>((stream, Quic { conn, cc, realtime, idle, groups, peer_groups: false }))
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
/// groups: what Video's groups share with it (groups.rs).
fn bridge(conn: &Connection, mut send: quinn::SendStream, mut recv: quinn::RecvStream, rate: Arc<AtomicU64>,
          realtime: Arc<AtomicU64>, groups: Arc<groups::Shared>) -> io::Result<UnixStream> {
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
                            conn.set_send_window(send_window(rate.load(Ordering::Relaxed), conn.rtt(), groups.going()));
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
                    if !write_after_groups(&mut send, &buf[..n], &groups).await {
                        break;
                    }
                }
            }
        }
        let _ = send.finish();
    });
    Ok(theirs)
}

/// data on the session's stream, all of it, a group's message waiting for
/// room in quinn's send window let go first (groups.rs YIELD at most, then
/// it goes: control messages go on): false if the stream is over.
async fn write_after_groups(send: &mut quinn::SendStream, mut data: &[u8], groups: &groups::Shared) -> bool {
    let mut yielding: Option<tokio::time::Instant> = None;
    while !data.is_empty() {
        let left = yielding.map_or(groups::YIELD, |since| groups::YIELD.saturating_sub(since.elapsed()));
        if left.is_zero() {
            match send.write(data).await {
                Ok(n) => data = &data[n..],
                Err(_) => return false,
            }
            yielding = None;
            continue;
        }
        let told = groups.notify.notified();
        tokio::pin!(told);
        told.as_mut().enable();
        if groups.waiting.load(Ordering::SeqCst) {
            yielding.get_or_insert_with(tokio::time::Instant::now);
            let _ = tokio::time::timeout(left, told).await;  // until it is written
            continue;
        }
        tokio::select! {
            written = send.write(data) => match written {
                Ok(n) => {
                    data = &data[n..];
                    yielding = None;
                }
                Err(_) => return false,
            },
            // A group's message began waiting: this write (none of it
            // written yet) gives way.
            _ = &mut told => {}
        }
    }
    true
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

    /// A dialed path and the tablet's end of it, the stream's first byte
    /// gone over (the tablet sees the stream only then).
    fn dialed() -> (UnixStream, Quic, UnixStream, Quic) {
        let pc = identity();
        let (tablet, address, paths) = tablet_for(&pc);
        let client = pc.client_config(only(tablet.fingerprint())).unwrap();
        let endpoint = endpoint(([127, 0, 0, 1], 0).into(), None).unwrap();
        let (mut stream, quic) = dial(&endpoint, &client, address, Duration::from_secs(3)).unwrap();
        stream.write_all(b"x").unwrap();
        let (theirs, far) = paths.recv_timeout(Duration::from_secs(3)).unwrap();
        (stream, quic, theirs, far)
    }

    /// What comes on far, datagrams and groups' messages, as it comes.
    fn read(far: &Quic) -> std::sync::mpsc::Receiver<Bytes> {
        let (came, got) = std::sync::mpsc::channel();
        let came = Mutex::new(came);
        far.read_datagrams(move |message| {
            let _ = came.lock().unwrap().send(message);
        });
        got
    }

    #[test]
    fn a_group_goes_on_a_stream_of_its_own_in_order() {
        let (_stream, mut quic, _theirs, far) = dialed();
        let got = read(&far);
        quic.set_groups(true);
        for i in 0..200u32 {
            let message = [&i.to_be_bytes()[..], &[7u8; 1000]].concat();
            assert!(quic.send_in_group(5, 1, false, i == 199, &message));
        }
        for i in 0..200u32 {
            let message = got.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!((message.len(), &message[..4]), (1004, &i.to_be_bytes()[..]));
        }
        let stats = quic.group_stats();
        assert_eq!((stats.groups, stats.late, stats.dropped, stats.stream_bytes), (1, 0, 0, 200 * 1006));
        assert_eq!(quic.conn.stats().frame_tx.datagram, 0, "a group's message as a datagram");
        assert_eq!(quic.queued(), 0);
    }

    /// The other side takes no group streams for a while (all MAX_STREAMS
    /// of them open): the newest group waits as long as it takes, an older
    /// one is given up LATE after it began, at once if a key group came
    /// after it; then the newest goes.
    fn groups_held_up(key: bool) -> (Vec<Bytes>, groups::Stats) {
        let (_stream, mut quic, _theirs, far) = dialed();
        quic.set_groups(true);
        for group in 0..groups::MAX_STREAMS {
            assert!(quic.send_in_group(5, group, false, true, &group.to_be_bytes()));
        }
        let held = groups::MAX_STREAMS;
        assert!(quic.send_in_group(5, held, false, true, b"held up"));
        std::thread::sleep(LATE + Duration::from_millis(100));
        let stats = quic.group_stats();
        assert_eq!((stats.late, stats.dropped), (0, 0), "the newest given up: {stats}");
        assert!(stats.credit_waits >= 1, "{stats}");
        assert!(quic.send_in_group(5, held + 1, key, true, b"newest"));
        std::thread::sleep(Duration::from_millis(50));
        let got = read(&far);
        let mut came = Vec::new();
        while let Ok(message) = got.recv_timeout(Duration::from_secs(1)) {
            came.push(message);
        }
        (came, quic.group_stats())
    }

    #[test]
    fn a_late_group_holds_up_no_newer_one_and_the_newest_is_never_late() {
        let (came, stats) = groups_held_up(false);
        assert_eq!(came.len() as u32, groups::MAX_STREAMS + 1, "{came:?}");
        assert_eq!(&came.last().unwrap()[..], b"newest");
        assert!(!came.iter().any(|m| &m[..] == b"held up"));
        assert_eq!((stats.late, stats.superseded, stats.dropped), (1, 0, 1), "{stats}");
    }

    #[test]
    fn a_key_group_supersedes_the_older_ones_at_once() {
        let (came, stats) = groups_held_up(true);
        assert_eq!(&came.last().unwrap()[..], b"newest");
        assert!(!came.iter().any(|m| &m[..] == b"held up"));
        assert_eq!((stats.late, stats.superseded, stats.dropped), (0, 1, 1), "{stats}");
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
