//! QUIC for the IP paths (architecture §3.14; chosen after the Stage 1
//! spike, link/quic-spike). A QUIC connection is a path of the session as a
//! TCP connection is: its first bidirectional stream carries the session's
//! frames, bridged to a socket pair as the cable's are (little room on the
//! session's end), and its datagrams the session's sealed realtime ones.
//! The congestion controller is ours (cc.rs; or the one PLUSONE_LINK_CC
//! chose, for a live A/B, under the same rules): the stream's bulk and the
//! datagrams share one rate, the datagrams first in each packet (quinn's
//! order), and realtime that would wait longer than LATE for the window is
//! dropped, not queued. A Video channel's groups (a video's frames) go on
//! unidirectional streams, one each, behind the datagrams, and copies of
//! the session's own messages on one more ahead of all (groups.rs); the
//! session's stream goes ahead of the groups while it carries no bulk,
//! behind them with it (bridge). The identities are the TCP path's (the same rustls
//! configs, pinned both ways). For now the session's own TLS still runs
//! inside the stream (sealed twice); a later step leaves it to QUIC's.
//! The endpoint's socket is ours (dscp.rs): its packets go as video
//! (AF41) while realtime goes, either way.

use crate::log;
use bytes::Bytes;
use quinn::{ClientConfig, Connection, ConnectionId, ConnectionIdGenerator, EndpointConfig, ServerConfig,
            TransportConfig};
use crate::cc::{Budget, CcConfig};
use crate::dscp::Marking;
use crate::gate::Gate;
use crate::groups::{self, Groups};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
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
/// group this long after its first byte went out, at least: reset; a newer
/// one's messages wait this long for it on the other side, or two round
/// trips: groups.rs).
pub(crate) const LATE: Duration = Duration::from_millis(100);
/// The queue the groups may hold past the path's round trip before the
/// application is told it is behind (queued()).
const QUEUE_OK: Duration = Duration::from_millis(20);
const DATAGRAM_SEND_BUFFER: usize = 512 << 10;
/// What quinn takes from the session's stream before it is acknowledged,
/// at first (about a fast path's bandwidth-delay product); then the
/// bridge has it follow the rate (WINDOW_EVERY).
pub(crate) const SEND_WINDOW: u64 = 2 << 20;
const BRIDGE_CHUNK: usize = 16 << 10;
/// What the bridge hands quinn ahead of the controller's rate: this long's
/// worth (at least AHEAD_LEAST). quinn keeps what it is handed in order,
/// whatever comes after: handed all at once (up to SEND_WINDOW), a control
/// message waited behind seconds of a transfer on a slow path, and an
/// opened channel was never answered. As TCP_NOTSENT_LOWAT on a TCP path.
const AHEAD: Duration = Duration::from_millis(30);
const AHEAD_LEAST: f64 = 32.0 * 1024.0;
/// What quinn's send window holds of the bulk past the rate's worth of the
/// path's least round trip (not the smoothed one: it grows with the queue
/// the window feeds): about the queue the bulk may make. As pace.rs holds a
/// TCP path's to a few ms, the realtime and the video wait behind it.
const WINDOW_AHEAD: Duration = Duration::from_millis(10);
/// How often quinn's send window follows the rate (Bridge): what it holds
/// unacknowledged, sent or not, the rate's worth of a round trip and AHEAD
/// (handed a quarter over the rate, it kept up to SEND_WINDOW unsent: a
/// second on a 15 Mbit/s path, and a channel's open waited behind it).
const WINDOW_EVERY: Duration = Duration::from_millis(10);
/// The session's stream carries bulk once more than this went on it in a
/// BULK_SPAN: it goes behind the groups then, ahead of them again QUIET
/// after the last (what quinn holds of the bulk gone by then, near enough).
const BULK_LEAST: usize = 8 << 10;
const BULK_SPAN: Duration = Duration::from_millis(100);
const QUIET: Duration = Duration::from_secs(1);
/// A connection ID: this many random bytes, then this many of their MAC
/// (MacCids).
const CID_NONCE: usize = 8;
const CID_MAC: usize = 4;
/// Over this many handshakes at once (gate.rs), an address proves it is
/// one (a Retry's token back) before its handshake costs anything, or
/// counts as its (architecture §3.14: under load, Retry is the cookie).
const RETRY_OVER: usize = 2;

/// The core's own runtime for quinn: one thread, whatever the program. A
/// connection's endpoint, driver and stream tasks only hand bytes on (the
/// crypto and the copies are short): it used 140 ms of CPU in 8 s of 20
/// Mbit/s of video, and a second thread only added wakes between them (a
/// third fewer context switches a packet on loopback with one, 45 % on
/// the small flows; and half again in the idle seconds).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).thread_name("link-quic").enable_all().build()
            .expect("a runtime for QUIC")
    })
}

fn transport(cc: Arc<CcConfig>) -> TransportConfig {
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
    // MAX_STREAMS at once, what comes on them sealed as the datagrams are,
    // and the control stream. Their data is bounded by the receive windows
    // (the connection's, 16 MiB; quinn has one window for every stream, the
    // session's 8 MiB). None while video streams are off (lib.rs).
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(if crate::video_streams() { groups::UNI_STREAMS } else { 0 }.into());
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
    cc: Arc<CcConfig>,
    /// Realtime bytes sent on it, all told: the bulk gets what they leave.
    realtime: Arc<AtomicU64>,
    /// quinn's datagram room with none waiting: what it holds is this less
    /// the room now. Not DATAGRAM_SEND_BUFFER: quinn-proto 0.11.17 keeps 32
    /// bytes aside, and 32 a datagram waiting; empty, it read 32 bytes still
    /// to send, and the desktop's wait for none never ended.
    idle: usize,
    /// Its endpoint's (dscp.rs).
    marking: Arc<Marking>,
    /// Video's groups on streams, and the session's messages on one of
    /// their own (groups.rs), if both hellos said so (set_streams).
    groups: Arc<Groups>,
    groups_on: bool,
    control_on: bool,
}

/// Why a realtime datagram did not go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unsent {
    /// It would have waited longer than LATE for the window.
    Late,
    /// quinn had no room for it, or would not take it.
    NoRoom,
}

/// Realtime a session dropped on its QUIC paths, said once a second at
/// most (the live tests set them against the desktop's frames): datagrams
/// late, those quinn had no room for, and the runs they came in (a run:
/// one channel's datagrams dropped one after the other, a frame's tail or
/// frames: the frames hit, roughly).
#[derive(Debug, Default)]
pub(crate) struct Drops {
    since: Option<Instant>,
    late: u64,
    room: u64,
    runs: u64,
    /// Each channel's last datagram: dropped or not.
    dropping: HashMap<u16, bool>,
}

impl Drops {
    /// A datagram on channel, sent (None) or not (why); the last second's
    /// line, once it is over and if anything was dropped in it.
    pub(crate) fn note(&mut self, channel: u16, unsent: Option<Unsent>, now: Instant) -> Option<String> {
        let since = *self.since.get_or_insert(now);
        match unsent {
            Some(why) => {
                if why == Unsent::Late { self.late += 1 } else { self.room += 1 }
                if !self.dropping.insert(channel, true).unwrap_or(false) {
                    self.runs += 1;
                }
            }
            None => {
                self.dropping.insert(channel, false);
            }
        }
        let over = now.duration_since(since);
        if over < Duration::from_secs(1) {
            return None;
        }
        let line = (self.late + self.room > 0).then(|| {
            format!("realtime dropped over {:.1} s: {} late, {} for room, in {} runs", over.as_secs_f64(),
                    self.late, self.room, self.runs)
        });
        (self.since, self.late, self.room, self.runs) = (Some(now), 0, 0, 0);
        line
    }
}

/// quinn's send window, all streams': the session stream's share, the
/// rate's worth of a round trip and AHEAD (the bulk ahead of the rate:
/// bridge), and the groups' bytes not all acknowledged yet (groups: they
/// never wait for room the bulk took; their own deadlines bound them).
pub(crate) fn send_window(rate: u64, rtt: Duration, groups: u64) -> u64 {
    ((rate as f64 * (rtt + WINDOW_AHEAD).as_secs_f64()) as u64).max(2 * BRIDGE_CHUNK as u64) + groups
}

/// Whether what quinn holds of datagrams (what its buffer, idle when
/// empty, has less of room) would take longer than LATE to go at the rate
/// (bytes a second; 0 before one is known).
fn waits_too_long(rate: u64, idle: usize, room: usize) -> bool {
    rate > 0 && idle.saturating_sub(room) as f64 > rate as f64 * LATE.as_secs_f64()
}

impl Quic {
    /// A sealed datagram out, unless it would wait longer than LATE for the
    /// window or does not fit. A ping or a pong (urgent) never waits for
    /// that: they are what says the path is alive and measures it, a few
    /// bytes each; held by the same rule as a full queue of video, three
    /// seconds of them lost made the session take its video off the
    /// datagrams.
    pub(crate) fn send(&self, sealed: &[u8], urgent: bool) -> Result<(), Unsent> {
        let rate = self.cc.rate.load(Ordering::Relaxed);
        let room = self.conn.datagram_send_buffer_space();
        if !urgent && waits_too_long(rate, self.idle, room) {
            return Err(Unsent::Late);
        }
        // Nor past quinn's room: it would drop the oldest to make some,
        // unseen (over about 42 Mbit/s, LATE's worth is more than it holds).
        if sealed.len() > room || self.conn.send_datagram(Bytes::copy_from_slice(sealed)).is_err() {
            return Err(Unsent::NoRoom);
        }
        self.realtime.fetch_add(sealed.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// The other side's address (its IP is where the side socket's
    /// datagrams go: session.rs raw).
    pub(crate) fn peer_ip(&self) -> std::net::IpAddr {
        self.conn.remote_address().ip()
    }

    /// Realtime went on it now, either way (input too): the endpoint's
    /// packets go as video (AF41) for a while (dscp.rs).
    pub(crate) fn realtime(&self) {
        self.marking.realtime();
    }

    /// The datagrams handed to quinn and not sent yet, and the groups'
    /// messages not yet on their streams (bytes).
    pub(crate) fn queued(&self) -> usize {
        self.idle.saturating_sub(self.conn.datagram_send_buffer_space()) + self.groups.queued() + self.group_backlog()
    }

    /// The groups' bytes written to quinn and not acknowledged, past what the
    /// path carries in its least round trip (and QUEUE_OK): in its send
    /// buffer, in the router's queue. Groups never wait for quinn's window
    /// (groups.rs), so without this the application saw no backlog on QUIC,
    /// and left out no frame, asked no keyframe to wait, however deep it was.
    fn group_backlog(&self) -> usize {
        let rate = self.cc.rate.load(Ordering::Relaxed) as f64;
        let carried = (rate * (self.groups.shared.base_rtt() + QUEUE_OK).as_secs_f64()) as u64;
        self.groups.shared.unacked().saturating_sub(carried) as usize
    }

    /// Video's groups on streams, and the session's messages on one of
    /// their own (both hellos say so).
    pub(crate) fn set_streams(&mut self, groups: bool, control: bool) {
        self.groups_on = groups;
        self.control_on = control;
    }

    pub(crate) fn groups(&self) -> bool {
        self.groups_on
    }

    pub(crate) fn control(&self) -> bool {
        self.control_on
    }

    /// A copy of frames the session numbered (its own messages) on the
    /// control stream (groups.rs), ahead of all.
    pub(crate) fn send_control(&self, frames: &[u8]) {
        self.groups.send_control(frames, |frames| {
            runtime().spawn(groups::write_control(self.conn.clone(), frames, self.groups.shared.clone()));
        });
    }

    /// A sealed message of channel's group (key: a key group; last: the
    /// group's last) to the group's stream (groups.rs): false if dropped.
    /// (Counted as realtime once handed over: the bulk leaves room for what
    /// the groups want, not only for what they could write.)
    pub(crate) fn send_in_group(&self, channel: u16, group: u32, key: bool, last: bool, sealed: &[u8]) -> bool {
        self.realtime.fetch_add((sealed.len() + 2) as u64, Ordering::Relaxed);
        self.groups.send(channel, group, key, last, sealed, |pieces| {
            runtime().spawn(groups::write(self.conn.clone(), pieces, self.groups.shared.clone()));
        })
    }

    /// channel's groups gone from this connection, group the last it began
    /// here (groups.rs Groups::leave): its stream reset if not all written.
    pub(crate) fn leave(&self, channel: u16, group: u32) {
        if let Some(telling) = self.groups.leave(channel, group) {
            runtime().spawn(telling);
        }
    }

    /// The groups so far, and the controller's losses and cuts.
    pub(crate) fn group_stats(&self) -> groups::Stats {
        let mut stats = self.groups.shared.stats();
        stats.datagram_bytes = self.realtime.load(Ordering::Relaxed).saturating_sub(self.groups.shared.handed());
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
        if stats.groups > 0 || stats.control > 0 {
            log!("{}: QUIC path over: {stats}", self.conn.remote_address());
        }
        self.conn.close(0u32.into(), b"path over");
    }

    /// Each datagram that comes, and each message of the other side's
    /// groups (groups.rs: in the order the groups began), to on; each frame
    /// on its control stream (kind, channel, body), to control, if we take
    /// one (on the runtime: neither must block).
    pub(crate) fn read_datagrams(&self, on: impl Fn(Bytes) + Send + Sync + 'static,
                                 control: impl Fn(u8, u16, Vec<u8>) + Send + Sync + 'static) {
        let on = Arc::new(on);
        let conn = self.conn.clone();
        let datagrams = on.clone();
        runtime().spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                datagrams(datagram);
            }
        });
        let control = self.control_on.then(|| Arc::new(control));
        runtime().spawn(groups::read(self.conn.clone(), on, control));
    }
}

fn client_config(client: &Arc<rustls::ClientConfig>, cc: CcConfig) -> Result<(ClientConfig, Arc<CcConfig>), String> {
    let mut tls = (**client).clone();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let cc = Arc::new(cc);
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
fn server_config(crypto: &Arc<quinn::crypto::rustls::QuicServerConfig>) -> (ServerConfig, Arc<CcConfig>) {
    let cc: Arc<CcConfig> = Arc::default();
    let mut config = ServerConfig::with_crypto(crypto.clone());
    config.transport_config(Arc::new(transport(cc.clone())));
    (config, cc)
}

/// A QUIC endpoint: quinn's, on our socket, and that socket's marking.
#[derive(Clone)]
pub(crate) struct Endpoint {
    quinn: quinn::Endpoint,
    marking: Arc<Marking>,
}

impl Endpoint {
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.quinn.local_addr()
    }

    pub(crate) fn close(&self) {
        self.quinn.close(0u32.into(), b"closed");
    }
}

/// An endpoint bound to address (port 0: any): it dials, and with server
/// configs it is dialed too.
pub(crate) fn endpoint(address: SocketAddr, server: Option<&Arc<rustls::ServerConfig>>) -> io::Result<Endpoint> {
    let server = match server {
        Some(server) => Some(server_config(&server_crypto(server).map_err(io::Error::other)?).0),
        None => None,
    };
    let socket = std::net::UdpSocket::bind(address)?;
    // Room for a keyframe's burst when the thread that reads is a moment late
    // (the default, about 200 KB, is lost to the kernel and read by the
    // controller as loss), as the session's own UDP path has.
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        crate::sys::set_int(std::os::fd::AsRawFd::as_raw_fd(&socket), libc::SOL_SOCKET, option, crate::session::UDP_BUFFER);
    }
    let key = ring::hmac::Key::generate(ring::hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
        .map_err(|_| io::Error::other("no random key"))?;
    let mut config = EndpointConfig::default();
    config.cid_generator(move || Box::new(MacCids(key.clone())));
    let _entered = runtime().enter();
    let marking = Arc::new(Marking::new());
    let quinn = if crate::dscp::on() {
        let socket = Arc::new(crate::dscp::Socket::new(socket, marking.clone())?);
        quinn::Endpoint::new_with_abstract_socket(config, server, socket, Arc::new(quinn::TokioRuntime))?
    } else {
        // quinn's own socket, as before dscp.rs: every packet DSCP 0.
        quinn::Endpoint::new(config, server, socket, Arc::new(quinn::TokioRuntime))?
    };
    Ok(Endpoint { quinn, marking })
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
    dial_with(endpoint, client, address, timeout, CcConfig::default())
}

/// As dial(), its controller cc.
fn dial_with(endpoint: &Endpoint, client: &Arc<rustls::ClientConfig>, address: SocketAddr, timeout: Duration,
             cc: CcConfig) -> Result<(UnixStream, Quic), String> {
    let (config, cc) = client_config(client, cc)?;
    runtime().block_on(async {
        let connecting = endpoint.quinn.connect_with(config, address, "plus-one.invalid").map_err(|e| e.to_string())?;
        let conn = tokio::time::timeout(timeout, connecting).await.map_err(|_| "no answer".to_owned())?
            .map_err(|e| e.to_string())?;
        // The other side sees the stream once something goes on it: the
        // session's TLS hello, which the dialer writes first.
        let (send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let realtime: Arc<AtomicU64> = Arc::default();
        let groups = Arc::new(Groups::new(&conn, &cc));
        let stream = bridge(&conn, send, recv, cc.rate.clone(), realtime.clone(), groups.shared.clone())
            .map_err(|e| e.to_string())?;
        let idle = conn.datagram_send_buffer_space();
        Ok((stream, Quic { conn, cc, realtime, idle, marking: endpoint.marking.clone(), groups, groups_on: false, control_on: false }))
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
        while let Some(incoming) = endpoint.quinn.accept().await {
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
            let marking = endpoint.marking.clone();
            tokio::spawn(async move {
                let from = incoming.remote_address();
                let accepted = async {
                    let conn = incoming.accept_with(Arc::new(config)).map_err(|e| e.to_string())?.await
                        .map_err(|e| e.to_string())?;
                    let (send, recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
                    let realtime: Arc<AtomicU64> = Arc::default();
                    let groups = Arc::new(Groups::new(&conn, &cc));
                    let stream = bridge(&conn, send, recv, cc.rate.clone(), realtime.clone(), groups.shared.clone())
                        .map_err(|e| e.to_string())?;
                    let idle = conn.datagram_send_buffer_space();
                    Ok::<_, String>((stream, Quic { conn, cc, realtime, idle, marking, groups, groups_on: false, control_on: false }))
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
        // The stream ahead of the groups while it carries no bulk: control
        // messages wait behind no frame (CONTROL...). The bytes in this
        // span, when bulk went last.
        let _ = send.set_priority(groups::SESSION_AHEAD);
        let (mut ahead, mut span, mut spanned, mut bulk) = (true, refilled, 0usize, None);
        loop {
            match from_session.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let now = tokio::time::Instant::now();
                    if now.duration_since(span) >= BULK_SPAN {
                        (span, spanned) = (now, 0);
                    }
                    spanned += n;
                    if spanned > BULK_LEAST {
                        bulk = Some(now);
                    }
                    let quiet = bulk.is_none_or(|at| now.duration_since(at) >= QUIET);
                    if quiet != ahead {
                        ahead = quiet;
                        let _ = send.set_priority(if ahead { groups::SESSION_AHEAD } else { groups::SESSION_BEHIND });
                    }
                    loop {
                        let now = tokio::time::Instant::now();
                        if now.duration_since(windowed) >= WINDOW_EVERY {
                            groups.set_window(&conn, send_window(rate.load(Ordering::Relaxed), groups.base_rtt(), groups.unacked()));
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
    fn realtime_dropped_is_said_once_a_second_with_its_runs() {
        let (mut drops, start) = (Drops::default(), Instant::now());
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert_eq!(drops.note(3, None, at(0)), None);
        // A frame's tail late, the sound beside it sent, then a whole frame late.
        for (ms, channel, unsent) in [(10, 3, Some(Unsent::Late)), (11, 3, Some(Unsent::Late)), (12, 5, None),
                                      (13, 3, Some(Unsent::Late)), (30, 3, None), (40, 3, Some(Unsent::NoRoom)),
                                      (41, 3, Some(Unsent::Late))] {
            assert_eq!(drops.note(channel, unsent, at(ms)), None);
        }
        assert_eq!(drops.note(5, None, at(1000)).as_deref(),
                   Some("realtime dropped over 1.0 s: 4 late, 1 for room, in 2 runs"));
        // A second with nothing dropped: nothing said.
        assert_eq!(drops.note(3, None, at(1500)), None);
        assert_eq!(drops.note(3, None, at(2100)), None);
        assert_eq!(drops.note(3, Some(Unsent::Late), at(2200)), None);
        assert!(drops.note(3, None, at(3200)).unwrap().contains(": 1 late, 0 for room, in 1 runs"));
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

    /// What comes on far, datagrams and groups' messages, as it comes; and
    /// its control stream's frames (kind, body).
    fn read_both(far: &Quic) -> (std::sync::mpsc::Receiver<Bytes>, std::sync::mpsc::Receiver<(u8, Vec<u8>)>) {
        let (came, got) = std::sync::mpsc::channel();
        let (framed, frames) = std::sync::mpsc::channel();
        let (came, framed) = (Mutex::new(came), Mutex::new(framed));
        far.read_datagrams(move |message| {
            let _ = came.lock().unwrap().send(message);
        }, move |kind, _, body| {
            let _ = framed.lock().unwrap().send((kind, body));
        });
        (got, frames)
    }

    fn read(far: &Quic) -> std::sync::mpsc::Receiver<Bytes> {
        read_both(far).0
    }

    #[test]
    fn a_group_goes_on_a_stream_of_its_own_in_order() {
        let (_stream, mut quic, _theirs, far) = dialed();
        let got = read(&far);
        quic.set_streams(true, false);
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
        // All acknowledged: no room held for it in quinn's send window, and no backlog.
        let started = Instant::now();
        while quic.groups.shared.unacked() > 0 {
            assert!(started.elapsed() < Duration::from_secs(3), "{} bytes held", quic.groups.shared.unacked());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(quic.queued(), 0);
    }

    #[test]
    fn what_the_groups_hold_past_the_rates_round_trip_is_queued() {
        let (_stream, mut quic, _theirs, far) = dialed();
        let _got = read(&far);
        quic.set_streams(true, false);
        // 100 KB/s: a round trip and QUEUE_OK carry a few KB of 200.
        quic.cc.rate.store(100_000, Ordering::Relaxed);
        for i in 0..200u32 {
            let message = [&i.to_be_bytes()[..], &[7u8; 1000]].concat();
            assert!(quic.send_in_group(5, 1, false, i == 199, &message));
        }
        let started = Instant::now();
        while quic.queued() < 100_000 && started.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(quic.queued() >= 100_000, "{} queued of 200 KB held at 100 KB/s", quic.queued());
    }

    /// What comes on got within wait of each other, until none does.
    fn all_of(got: &std::sync::mpsc::Receiver<Bytes>, wait: Duration) -> Vec<Vec<u8>> {
        let mut came = Vec::new();
        while let Ok(message) = got.recv_timeout(wait) {
            came.push(message.to_vec());
        }
        came
    }

    #[test]
    fn the_other_sides_groups_come_in_the_order_they_began() {
        // Channel 5's group begins; channel 6's begins after it and is whole
        // before the first has its tail: handed on after it all the same (a
        // frame whole after a newer one is lost to the app). Each stream's
        // task handing its own on, the newer one's went first.
        let (_stream, mut quic, _theirs, far) = dialed();
        let got = read(&far);
        quic.set_streams(true, false);
        assert!(quic.send_in_group(5, 1, false, false, &[b'a', 0]));
        for i in 0..5u8 {
            assert!(quic.send_in_group(6, 1, false, i == 4, &[b'b', i]));
        }
        std::thread::sleep(Duration::from_millis(30));
        for i in 1..5u8 {
            assert!(quic.send_in_group(5, 1, false, i == 4, &[b'a', i]));
        }
        let came = all_of(&got, Duration::from_millis(500));
        let ordered: Vec<Vec<u8>> = (0..5).map(|i| vec![b'a', i]).chain((0..5).map(|i| vec![b'b', i])).collect();
        assert_eq!(came, ordered);
        let stats = quic.group_stats();
        assert_eq!((stats.groups, stats.late, stats.stopped, stats.dropped), (2, 0, 0, 0), "{stats}");
    }

    #[test]
    fn a_newer_group_waits_late_at_most_for_an_older_one_then_that_one_is_given_up() {
        // Channel 5's group never ends (its sender held up); channel 6's is
        // whole after it began: handed on LATE after it came, channel 5's
        // stopped then, what still came on it dropped and its sender told.
        let (_stream, mut quic, _theirs, far) = dialed();
        let got = read(&far);
        quic.set_streams(true, false);
        assert!(quic.send_in_group(5, 1, false, false, b"a0"));
        assert_eq!(&got.recv_timeout(Duration::from_millis(500)).unwrap()[..], b"a0");
        let sent = Instant::now();
        assert!(quic.send_in_group(6, 1, false, true, b"b0"));
        assert_eq!(&got.recv_timeout(Duration::from_millis(500)).unwrap()[..], b"b0");
        let waited = sent.elapsed();
        assert!(waited >= LATE - Duration::from_millis(20) && waited < LATE + Duration::from_millis(150),
                "the newer group waited {waited:?}");
        std::thread::sleep(Duration::from_millis(50));
        assert!(quic.send_in_group(5, 1, false, true, b"a1"));
        assert!(got.recv_timeout(Duration::from_millis(300)).is_err(), "a given up group's message came");
        let stats = quic.group_stats();
        assert_eq!((stats.stopped, stats.late, stats.dropped), (1, 0, 1), "{stats}");
    }

    /// The other side takes no group streams for a while (all UNI_STREAMS
    /// of them open): the newest group waits as long as it takes, an older
    /// one is given up LATE after it began, at once if a key group came
    /// after it; then the newest goes.
    fn groups_held_up(key: bool) -> (Vec<Bytes>, groups::Stats) {
        let (_stream, mut quic, _theirs, far) = dialed();
        quic.set_streams(true, false);
        for group in 0..groups::UNI_STREAMS {
            assert!(quic.send_in_group(5, group, false, true, &group.to_be_bytes()));
        }
        // All of them on their streams (a loaded machine is slow to).
        let started = Instant::now();
        while quic.group_stats().groups < groups::UNI_STREAMS as u64 || quic.queued() > 0 {
            assert!(started.elapsed() < Duration::from_secs(5), "{}", quic.group_stats());
            std::thread::sleep(Duration::from_millis(10));
        }
        let held = groups::UNI_STREAMS;
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
        assert_eq!(came.len() as u32, groups::UNI_STREAMS + 1, "{came:?}");
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
    fn a_group_and_a_control_frame_never_wait_for_room_in_quinns_send_window() {
        // The window full (the bulk's: here none at all): each written all
        // the same, the window opened for it and set back (the bulk's share
        // stays its own). They waited a round trip at least for the bulk's
        // bytes to be acknowledged, past LATE under load.
        let (_stream, mut quic, _theirs, mut far) = dialed();
        far.set_streams(true, true);
        let (got, frames) = read_both(&far);
        quic.set_streams(true, true);
        quic.groups.shared.set_window(&quic.conn, 0);
        for i in 0..20u32 {
            let message = [&i.to_be_bytes()[..], &[7u8; 1000]].concat();
            assert!(quic.send_in_group(5, i / 10, false, i % 10 == 9, &message));
        }
        let mut control = Vec::new();
        for i in 0..3u8 {
            crate::frame::encode(crate::frame::SESSION, 0, &[i; 9], &mut control);
        }
        quic.send_control(&control[..16]);  // frames cut anywhere still come whole
        quic.send_control(&control[16..]);
        // (Each group's in order; the other side reads each stream apart.)
        let mut came: Vec<u32> = (0..20).map(|_| {
            let message = got.recv_timeout(Duration::from_millis(500)).expect("a group's message held up");
            u32::from_be_bytes(message[..4].try_into().unwrap())
        }).collect();
        for g in 0..2u32 {
            let group: Vec<u32> = came.iter().copied().filter(|i| i / 10 == g).collect();
            assert!(group.windows(2).all(|w| w[0] < w[1]), "{group:?}");
        }
        came.sort_unstable();
        assert_eq!(came, (0..20).collect::<Vec<u32>>());
        for i in 0..3u8 {
            let (kind, body) = frames.recv_timeout(Duration::from_millis(500)).expect("a control frame held up");
            assert_eq!((kind, body), (crate::frame::SESSION, vec![i; 9]));
        }
        let stats = quic.group_stats();
        assert!(stats.window_full >= 1 && stats.wait_ms == 0, "{stats}");
        assert_eq!(*quic.groups.shared.window.lock().unwrap(), 0, "the window not set back");
    }

    #[test]
    fn a_control_stream_to_a_side_that_takes_none_is_stopped() {
        let (_stream, mut quic, _theirs, far) = dialed();
        let (_got, frames) = read_both(&far);  // far's hello said nothing of it
        quic.set_streams(false, true);
        let mut control = Vec::new();
        crate::frame::encode(crate::frame::SESSION, 0, b"12345678{}", &mut control);
        quic.send_control(&control);
        assert!(frames.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn a_datagram_waits_too_long_past_late_of_the_rate() {
        // 1 MB/s: 100 KB is LATE's worth.
        assert!(!waits_too_long(1_000_000, 500_000, 500_000), "an empty queue");
        assert!(!waits_too_long(1_000_000, 500_000, 400_000), "100 KB is LATE's worth: not past it");
        assert!(waits_too_long(1_000_000, 500_000, 399_000));
        assert!(!waits_too_long(0, 500_000, 0), "no rate known yet");
        assert!(!waits_too_long(1_000_000, 100, 500), "more room than the buffer had: nothing waits");
    }

    #[test]
    fn each_controller_carries_the_stream_and_tells_a_budget() {
        let pc = identity();
        let (tablet, address, paths) = tablet_for(&pc);
        let client = pc.client_config(only(tablet.fingerprint())).unwrap();
        let endpoint = endpoint(([127, 0, 0, 1], 0).into(), None).unwrap();
        for name in crate::cc::NAMES {
            let cc = CcConfig::with(crate::cc::Choice::parse(Some(name), None).0);
            let (mut stream, quic) = dial_with(&endpoint, &client, address, Duration::from_secs(3), cc).unwrap();
            let sent: Vec<u8> = (0..2u32 << 20).map(|i| (i % 249) as u8).collect();
            let writer = {
                let sent = sent.clone();
                std::thread::spawn(move || {
                    stream.write_all(&sent).unwrap();
                    // The budget is told first PUBLISH_EVERY after the start:
                    // more past it (on loopback the rest took less).
                    std::thread::sleep(Duration::from_millis(150));
                    stream.write_all(&sent[..64 << 10]).unwrap();
                    stream
                })
            };
            let (mut theirs, _) = paths.recv_timeout(Duration::from_secs(3)).unwrap();
            theirs.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut came = vec![0u8; sent.len() + (64 << 10)];
            let started = Instant::now();
            theirs.read_exact(&mut came).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(came[..sent.len()] == sent[..] && came[sent.len()..] == sent[..64 << 10], "{name}");
            assert!(started.elapsed() < Duration::from_secs(10), "{name}: {:?}", started.elapsed());
            // A datagram goes too, under the same window.
            assert!(quic.send(&[1u8; 1000], false).is_ok(), "{name}: a datagram refused");
            let (told, rate) = quic.budget().last();
            assert!(told >= 1 && rate >= 30_000 && quic.rate() >= 30_000, "{name}: {told} told, {rate}");
            drop(writer.join().unwrap());
            quic.close();
        }
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
