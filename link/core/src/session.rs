//! A session with a paired device: its keys, its channels and their
//! reliable messages, over one path at a time, which may change while the
//! session lasts (the cable plugged in or pulled, the Wi-Fi lost and back).
//!
//! A path: a TLS 1.3 stream (the session's messages, the control
//! channels; input and realtime too while there is no UDP), and, when
//! there is IP under it, a UDP socket or the QUIC connection's datagrams
//! (input and realtime, sealed with the session's keys from the first TLS
//! exporter). A path lost, the session
//! waits RESUME_WAIT for another (the dialer makes one: plus-one-link);
//! a better one coming (the cable) takes over. Each side numbers its
//! reliable frames (the session's messages, the control channels'); a ping
//! says up to which the other's came, and what a lost path did not
//! deliver is sent again on the next, the duplicates dropped.
//!
//! Threads, none of them busy while nothing moves:
//! - the session's: it alone touches the path's TLS connection; reads,
//!   writes what the others queued (woken by an eventfd), pings each
//!   second, takes a path handed over, waits for one when it is lost;
//! - the path's datagrams': opens what arrives and hands it to its channel
//!   (a QUIC path's, on quinn's runtime: quic.rs);
//! - one per channel: what the feature writes, to the other side.

use crate::frame::{self, Decoder};
use crate::node::NodeInner;
use crate::seal::{Opener, Sealer};
use crate::stream::Stream;
use crate::sys::{self, Wake};
use crate::{log, Class};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A ping each this long, on the stream and over UDP.
pub const PING_EVERY: Duration = Duration::from_secs(1);
/// Nothing from the other side on a path this long (its stream and its
/// datagrams alike): the path is lost.
pub const PATH_SILENCE: Duration = Duration::from_secs(5);
/// No path this long: the session is over.
pub const RESUME_WAIT: Duration = Duration::from_secs(15);
/// The same with a channel open (the desktop, the speakers...): time for
/// both devices to be moved to another network by hand.
pub const RESUME_WAIT_IN_USE: Duration = Duration::from_secs(60);
/// A pong this recent over UDP: datagrams go both ways.
const UDP_PROOF: Duration = Duration::from_secs(3);
/// While UDP is not proven yet, a ping this often (the first ones sooner).
const UDP_PROBE: Duration = Duration::from_millis(200);
/// No room in the UDP socket: this long for some, then the packet is lost
/// (a keyframe's tail where the system caps the socket's room).
const UDP_STALL: Duration = Duration::from_millis(20);
const HANDSHAKE: Duration = Duration::from_secs(5);
const OPEN_WAIT: Duration = Duration::from_secs(5);
/// An input or realtime message's largest size: sealed (30 bytes more), it
/// fits a QUIC datagram on a 1500-byte network (QUIC's header, tag and
/// frame take about 40 of the 1452 bytes IP and UDP leave), and a UDP one.
pub const MAX_DATAGRAM: usize = 1380;
/// Realtime over the stream: dropped while this much urgent waits to be
/// sent, at most; on a slow stream, while more than REALTIME_WAIT of it
/// does at the rate the stream takes it (Bluetooth: 256 KiB were seconds).
const STREAM_BUDGET: usize = 256 << 10;
const REALTIME_WAIT: Duration = Duration::from_millis(150);
/// The stream's rate is measured over this long, while it is full.
const RATE_WINDOW: Duration = Duration::from_millis(200);
/// Reliable frames handed to TLS at a time: an urgent frame (input,
/// realtime, a ping) queued meanwhile waits behind no more than this.
const WRITE_SLICE: usize = 16 << 10;
/// The datagrams' DSCP, AF41 (34), as the IP header's byte.
const REALTIME_TOS: libc::c_int = 34 << 2;
/// Room in the UDP socket, each way: a keyframe's packets at once.
const UDP_BUFFER: libc::c_int = 4 << 20;
/// Room in a channel's socket, each way (the system caps it: a control
/// message is at most about 200 KB; more goes on a bulk channel).
const CHANNEL_BUFFER: usize = 1 << 20;
/// A control message with no room in its channel: this long for the
/// feature to read, then the channel is closed (it holds up the stream).
const CONTROL_STALL: Duration = Duration::from_secs(1);
/// A control channel's window, with a peer that says "flow" in its hello:
/// what its sender may have sent beyond what the other side's feature has
/// taken. The receiving side keeps what comes in a queue of its own (never
/// blocking the session, never closing a channel for being slow), and says
/// how much its feature has taken ("credit", a running total) each
/// GRANT_EVERY. A control message is at most about 200 KB (the sockets'
/// own limit): far under WINDOW - GRANT_EVERY, so a sender never waits on
/// credit that is held back.
const WINDOW: u64 = 1 << 20;
const GRANT_EVERY: u64 = 64 << 10;
/// A control message waits for the queue to drain below this before it
/// joins it: a big transfer (a PDF, the app) feeds the stream as fast as
/// the stream goes, never ahead of it, so what shares the stream (the
/// cable's realtime packets, under STREAM_BUDGET) never waits behind more.
const CONTROL_BACKLOG: usize = 128 << 10;
/// Channels a session may have at once (architecture §3.13); one more is
/// refused, either side's.
const MAX_CHANNELS: usize = 64;
/// Realtime counts as going this long after its last datagram (pace.rs).
const REALTIME_GOING: Duration = Duration::from_millis(500);
/// The path's pings this often while realtime and bulk go together
/// (pace.rs's samples), not each PING_EVERY.
const PROBE_BUSY: Duration = Duration::from_millis(40);
/// How often a control message waiting for room looks again: only while
/// a transfer is ahead of the path.
const ROOM_POLL: Duration = Duration::from_millis(2);
/// Reliable frames taken since the last ping that make one go at once.
const ACK_AFTER: u64 = 32;
/// Reliable bytes sent and not yet acknowledged, at most: past it no more
/// go until the other side says it has them (it acknowledges each
/// ACK_AFTER frames, 32 MiB at the very most; one that never does holds no
/// more of ours than this).
const UNACKED_MOST: usize = 64 << 20;
const KEYS_LABEL: &[u8] = b"EXPORTER-plus-one-link datagrams";
const ID_LABEL: &[u8] = b"EXPORTER-plus-one-link session";
const DATAGRAM_PATH: u16 = 0;  // the datagrams' own channel: pings and pongs
const PING: u8 = 1;
const PONG: u8 = 2;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// How good a path is: a better one takes over from a worse.
fn rank(kind: &str) -> u8 {
    match kind {
        "usb" => 4,
        "p2p" | "ap" => 3,  // Wi-Fi Direct, the PC's own access point: once through the air, no one else's queue
        "lan" => 2,
        "bt" => 1,  // Bluetooth: slow, but with no network at all
        _ => 0,     // the relay
    }
}

/// A channel's end, the feature's: a message socket. Close it (drop) and
/// the channel closes; its end (a read of 0) is the channel closed by the
/// other side, or the session over.
pub struct Channel {
    fd: OwnedFd,
    pub id: u16,
    pub class: Class,
    pub feature: String,
    pub name: String,
}

impl Channel {
    /// The same channel, for another thread (one reads while one writes).
    pub fn try_clone(&self) -> io::Result<Channel> {
        Ok(Channel { fd: self.fd.try_clone()?, id: self.id, class: self.class, feature: self.feature.clone(),
                     name: self.name.clone() })
    }

    /// One message, waiting for room.
    pub fn send(&self, data: &[u8]) -> io::Result<()> {
        sys::send(self.fd.as_raw_fd(), data, true).map(|_| ())
    }

    /// One message into buf: Some(its length), Some(0) at the end, None if
    /// nothing came within timeout.
    pub fn recv(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<Option<usize>> {
        if !sys::readable(self.fd.as_raw_fd(), timeout)? {
            return Ok(None);
        }
        let length = sys::recv(self.fd.as_raw_fd(), buf)?;
        if length > buf.len() {
            return Err(invalid(format!("a message of {length} bytes")));
        }
        Ok(Some(length))
    }

    /// The descriptor, for whoever reads and writes it now (plus-one-link
    /// hands it to an app).
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsRawFd for Channel {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// A channel the other side opened, for a feature we serve.
pub struct Incoming {
    pub session: Arc<Session>,
    pub channel: Channel,
}

/// The session's end of a channel.
struct End {
    id: u16,
    class: Class,
    fd: OwnedFd,
    /// Input: the newest datagram's counter handed on, plus one (0: none).
    newest: AtomicU64,
    /// Closed: a pump waiting for credit gives up.
    gone: AtomicBool,
    /// Flow (control, with a peer that has it): bytes sent on it, and the
    /// other side's running total taken by its feature.
    sent: AtomicU64,
    granted: AtomicU64,
    /// Flow: what comes for it, to its feature in order on a thread of its
    /// own (None: the channel's end, after what came before it); bytes
    /// come, and taken by the feature.
    inbox: Mutex<Option<mpsc::Sender<Option<Vec<u8>>>>>,
    received: AtomicU64,
    taken: AtomicU64,
}

impl End {
    fn new(id: u16, class: Class, fd: OwnedFd) -> End {
        End { id, class, fd, newest: AtomicU64::new(0), gone: AtomicBool::new(false), sent: AtomicU64::new(0),
              granted: AtomicU64::new(0), inbox: Mutex::new(None), received: AtomicU64::new(0),
              taken: AtomicU64::new(0) }
    }
}

/// A reliable frame not numbered yet: it is numbered as it leaves
/// (Outbox::take), in the order frames leave, so the queue may put one
/// ahead of others (the other side takes numbers in order only).
struct Waiting {
    kind: u8,
    channel: u16,
    body: Vec<u8>,
}

impl Waiting {
    fn size(&self) -> usize {
        frame::HEADER + 8 + self.body.len()
    }
}

/// Where a reliable frame waits.
#[derive(Clone, Copy)]
enum Queue {
    /// The session's own messages (opens and their answers, credits): ahead
    /// of every channel's, never behind a transfer (an opened channel went
    /// unanswered behind seconds of one on a slow path).
    Ahead,
    /// A channel's: in its order, the channels in turn (a message on one
    /// waits behind no other's transfer); a channel's close after its data.
    Channel(u16),
    /// After everything ("bye").
    Last,
}

/// What waits to go on the stream: the urgent (input and realtime frames,
/// pings) never wait behind the reliable (control messages, a big
/// transfer's), which go to TLS frame by frame, a slice at a time; between
/// two slices the urgent go first.
#[derive(Default)]
struct Outbox {
    urgent: Vec<u8>,
    /// Numbered already: what a lost path had not delivered, first again.
    resend: VecDeque<Vec<u8>>,
    ahead: VecDeque<Waiting>,
    channels: BTreeMap<u16, (VecDeque<Waiting>, usize)>,
    last: VecDeque<Waiting>,
    /// The channel whose turn comes next.
    turn: u16,
    reliable_bytes: usize,
}

impl Outbox {
    fn len(&self) -> usize {
        self.urgent.len() + self.reliable_bytes
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What one channel has waiting (bytes).
    fn channel_len(&self, id: u16) -> usize {
        self.channels.get(&id).map_or(0, |(_, bytes)| *bytes)
    }

    fn push(&mut self, waiting: Waiting, queue: Queue) {
        self.reliable_bytes += waiting.size();
        match queue {
            Queue::Ahead => self.ahead.push_back(waiting),
            Queue::Last => self.last.push_back(waiting),
            Queue::Channel(id) => {
                let (frames, bytes) = self.channels.entry(id).or_default();
                *bytes += waiting.size();
                frames.push_back(waiting);
            }
        }
    }

    /// A new path: the urgent are stale; what the last one had not
    /// delivered (numbered) goes first again, the rest as it was.
    fn restart(&mut self, unacked: impl Iterator<Item = Vec<u8>>) {
        self.reliable_bytes -= self.resend.iter().map(Vec::len).sum::<usize>();
        self.urgent.clear();
        self.resend = unacked.collect();
        self.reliable_bytes += self.resend.iter().map(Vec::len).sum::<usize>();
    }

    /// The next channel's frame, the channels in turn.
    fn next_channel(&mut self) -> Option<Waiting> {
        let id = self.channels.range(self.turn..).chain(self.channels.range(..self.turn)).map(|(id, _)| *id).next()?;
        let (frames, bytes) = self.channels.get_mut(&id)?;
        let waiting = frames.pop_front()?;
        *bytes -= waiting.size();
        if frames.is_empty() {
            self.channels.remove(&id);
        }
        self.turn = id.wrapping_add(1);
        Some(waiting)
    }

    /// The next for TLS: every urgent frame and those to send again, then
    /// reliable frames up to reliable bytes (0: none now) to be numbered;
    /// and how many bytes of the reliable.
    fn take(&mut self, reliable: usize) -> (Vec<u8>, Vec<Waiting>, usize) {
        let mut out = std::mem::take(&mut self.urgent);
        let (mut fresh, mut taken) = (Vec::new(), 0);
        while taken < reliable {
            if let Some(bytes) = self.resend.pop_front() {
                taken += bytes.len();
                self.reliable_bytes -= bytes.len();
                out.extend_from_slice(&bytes);
                continue;
            }
            let next = self.ahead.pop_front().or_else(|| self.next_channel()).or_else(|| self.last.pop_front());
            let Some(waiting) = next else { break };
            taken += waiting.size();
            self.reliable_bytes -= waiting.size();
            fresh.push(waiting);
        }
        (out, fresh, taken)
    }
}

/// The reliable frames: ours numbered, kept until acknowledged; the
/// other side's newest taken.
#[derive(Default)]
struct Reliable {
    next: u64,
    unacked: VecDeque<(u64, Vec<u8>)>,
    unacked_bytes: usize,
    heard: u64,
    acknowledged: u64,  // the newest of theirs we said we have
}

/// A path the session runs on now.
struct Path {
    kind: String,
    stream: Box<dyn Stream>,  // a clone of the session thread's, to shut it down
    udp: Option<UdpSocket>,
    udp_peer: Mutex<Option<SocketAddr>>,
    /// A QUIC path's connection: its datagrams instead of UDP's.
    quic: Option<crate::quic::Quic>,
    started: Instant,
    heard_us: AtomicU64,   // since started: the stream's last frame, or an authenticated datagram
    proven_us: AtomicU64,  // a pong's (0: none yet)
    rtt_us: AtomicU64,
    realtime_us: AtomicU64,  // since started: the last realtime or input datagram sent (0: none)
    gone: AtomicBool,
}

impl Path {
    fn us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    fn heard(&self) {
        self.heard_us.store(self.us().max(1), Ordering::Relaxed);
    }

    /// Realtime or input went on it within the last REALTIME_GOING.
    fn realtime_going(&self) -> bool {
        let last = self.realtime_us.load(Ordering::Relaxed);
        last != 0 && self.us().saturating_sub(last) < REALTIME_GOING.as_micros() as u64
    }

    fn udp_works(&self) -> bool {
        let proven = self.proven_us.load(Ordering::Relaxed);
        (self.udp.is_some() || self.quic.is_some()) && proven != 0
            && self.us().saturating_sub(proven) < UDP_PROOF.as_micros() as u64
    }
}

pub struct Session {
    /// The other side's certificate's fingerprint, and what its hello said.
    pub peer: String,
    pub name: String,
    /// The session's, the same on both sides: a path for it names it.
    pub id: String,
    features: Mutex<Value>,
    node: Weak<NodeInner>,
    closed: AtomicBool,
    /// Said "bye": the path's end that follows is not a path lost.
    closing: AtomicBool,
    channels: Mutex<HashMap<u16, Arc<End>>>,
    pending: Mutex<HashMap<u16, mpsc::Sender<Result<(), String>>>>,
    next_id: AtomicU16,
    /// The other side has flow control on control channels (WINDOW).
    flow: bool,
    /// The reliable frames' pace on the current path (pace.rs).
    pace: Mutex<crate::pace::Pace>,
    /// How much urgent may wait on the stream before realtime is dropped.
    stream_budget: std::sync::atomic::AtomicUsize,
    outbox: Mutex<Outbox>,
    reliable: Mutex<Reliable>,
    wake: Wake,
    path: Mutex<Option<Arc<Path>>>,
    arriving: Mutex<Option<Established>>,
    sealer: Sealer,
    opener: Mutex<Opener>,
    dropped: AtomicU64,
}

/// What a connection's TLS handshake and hellos left: a new session
/// starts from it, or a session takes it as its path.
pub(crate) struct Established {
    conn: rustls::Connection,
    stream: Box<dyn Stream>,
    decoder: Decoder,
    pub(crate) peer: String,
    hello: Value,
    udp: Option<UdpSocket>,
    udp_peer: Option<SocketAddr>,
    sealer: Sealer,
    opener: Opener,
    dialer: bool,
    kind: String,
    id: String,
    /// The session both hellos named (the same one): this is a path for it.
    pub(crate) resumes: Option<String>,
    /// The other side has no path for it now: this one is taken, whatever
    /// we believe of ours (it is gone for them).
    forced: bool,
    /// A QUIC path's connection (quic.rs).
    quic: Option<crate::quic::Quic>,
}

impl Established {
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    /// The connection under the stream is QUIC's: its datagrams carry ours.
    pub(crate) fn set_quic(&mut self, quic: crate::quic::Quic) {
        self.quic = Some(quic);
    }

    /// Where to make a QUIC path to the other side, if this is a TCP one we
    /// made and its hello says it takes QUIC.
    pub(crate) fn quic_offer(&self) -> Option<SocketAddr> {
        let port = self.hello["quic"].as_u64().filter(|p| (1..=65535).contains(p))?;
        match (self.dialer, &self.quic, self.stream.addresses()) {
            (true, None, Some((peer, _))) if peer.is_ipv4() => Some(SocketAddr::new(peer, port as u16)),
            _ => None,
        }
    }

    fn close(&self) {
        self.stream.shutdown();
        if let Some(quic) = &self.quic {
            quic.close();
        }
    }
}

/// The TLS handshake, the keys and the hellos, on a new connection. The
/// verifier has checked the other side is paired. kind: what it runs on;
/// ours: the session we have with that device, if any (to go on with it:
/// both hellos name it, or a new one starts), and whether it has no path.
pub(crate) fn establish(mut conn: rustls::Connection, mut stream: Box<dyn Stream>, dialer: bool, hello: Value,
                        kind: &str, ours: &dyn Fn(&str) -> Option<(String, bool)>) -> io::Result<Established> {
    stream.set_timeouts(Some(HANDSHAKE))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut stream)?;
    }
    let peer = conn.peer_certificates().and_then(|c| c.first()).map(|c| crate::fingerprint(c))
        .ok_or_else(|| invalid("no certificate"))?;
    let keys = conn.export_keying_material([0u8; 64], KEYS_LABEL, None).map_err(|e| invalid(e.to_string()))?;
    let id_bytes = conn.export_keying_material([0u8; 16], ID_LABEL, None).map_err(|e| invalid(e.to_string()))?;
    let id: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let (mine, theirs) = if dialer { (0, 32) } else { (32, 0) };
    let (send_direction, receive_direction) =
        if dialer { (crate::seal::DIALER, crate::seal::LISTENER) } else { (crate::seal::LISTENER, crate::seal::DIALER) };
    let sealer = Sealer::new(keys[mine..mine + 32].try_into().unwrap(), send_direction);
    let opener = Opener::new(keys[theirs..theirs + 32].try_into().unwrap(), receive_direction);

    // Datagrams as UDP when there is IP under the stream.
    let udp = match stream.addresses() {
        Some((_, local)) => {
            let udp = UdpSocket::bind(SocketAddr::new(local, 0))?;
            // Input and realtime ahead of the stream's bulk on the air: DSCP
            // AF41, which Wi-Fi puts in its video queue (WMM AC_VI, RFC
            // 8325); the stream stays best effort.
            if local.is_ipv4() {
                sys::set_int(udp.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS, REALTIME_TOS);
            } else {
                sys::set_int(udp.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_TCLASS, REALTIME_TOS);
            }
            // As much as the system allows (net.core.wmem_max, rmem_max).
            sys::set_int(udp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, UDP_BUFFER);
            sys::set_int(udp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, UDP_BUFFER);
            Some(udp)
        }
        None => None,
    };
    let (resume, detached) = match ours(&peer) {
        Some((id, detached)) => (Some(id), detached),
        None => (None, false),
    };
    let mut hello = hello;
    if let Some(udp) = &udp {
        hello["udp"] = json!(udp.local_addr()?.port());
    }
    hello["resume"] = json!(resume);
    hello["detached"] = json!(detached);
    // What the path is: the side that made it knows (a TCP connection
    // accepted could be the LAN's or a Wi-Fi Direct group's or the PC's own
    // network's, all alike to the side taking it).
    hello["path"] = json!(kind);
    let mut bytes = Vec::new();
    let mut body = 0u64.to_be_bytes().to_vec();  // numbered 0: before the session's own
    body.extend_from_slice(hello.to_string().as_bytes());
    frame::encode(frame::SESSION, 0, &body, &mut bytes);
    conn.writer().write_all(&bytes)?;
    while conn.wants_write() {
        conn.write_tls(&mut stream)?;
    }
    let mut decoder = Decoder::default();
    let theirs = loop {
        if let Some((kind, _, body)) = read_frame(&mut conn, &mut stream, &mut decoder)? {
            if kind != frame::SESSION || body.len() < 8 {
                return Err(invalid("a frame before the hello"));
            }
            let message: Value = serde_json::from_slice(&body[8..]).map_err(|e| invalid(e.to_string()))?;
            if message["t"] != "hello" {
                return Err(invalid("no hello"));
            }
            break message;
        }
    };
    let port = theirs["udp"].as_u64().filter(|p| (1..=65535).contains(p));
    let udp_peer = match (stream.addresses(), port) {
        (Some((peer, _)), Some(port)) if udp.is_some() => Some(SocketAddr::new(peer, port as u16)),
        _ => None,
    };
    let udp = if udp_peer.is_some() { udp } else { None };
    let resumes = resume.filter(|r| theirs["resume"].as_str() == Some(r.as_str()));
    let forced = theirs["detached"].as_bool().unwrap_or(false);
    // Taken, a TCP path is what its dialer says (both sides authenticated by now).
    let kind = match theirs["path"].as_str() {
        Some(said @ ("p2p" | "ap")) if !dialer && kind == "lan" => said.to_owned(),
        _ => kind.to_owned(),
    };
    Ok(Established { conn, stream, decoder, peer, hello: theirs, udp, udp_peer, sealer, opener, dialer,
                     kind, id, resumes, forced, quic: None })
}

/// A connection made ready to be a path: non-blocking from now on, its
/// UDP socket (if any) taken into the path.
fn prepare(established: &mut Established) -> io::Result<Arc<Path>> {
    established.stream.set_timeouts(None)?;
    established.stream.set_nonblocking(true)?;
    let udp = established.udp.take();
    if let Some(udp) = &udp {
        udp.set_read_timeout(Some(Duration::from_millis(500)))?;
    }
    Ok(Arc::new(Path { kind: established.kind.clone(), stream: established.stream.try_clone()?, udp,
                       udp_peer: Mutex::new(established.udp_peer), quic: established.quic.take(),
                       started: Instant::now(), heard_us: AtomicU64::new(1),
                       proven_us: AtomicU64::new(0), rtt_us: AtomicU64::new(0), realtime_us: AtomicU64::new(0),
                       gone: AtomicBool::new(false) }))
}

/// Blocking, during the hellos: the next frame (None: read more).
fn read_frame(conn: &mut rustls::Connection, stream: &mut Box<dyn Stream>, decoder: &mut Decoder)
              -> io::Result<Option<(u8, u16, Vec<u8>)>> {
    if let Some(frame) = decoder.next()? {
        return Ok(Some(frame));
    }
    let mut plain = [0u8; 16 << 10];
    match conn.reader().read(&mut plain) {
        Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(n) => {
            decoder.push(&plain[..n]);
            return Ok(None);
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    if conn.read_tls(stream)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    conn.process_new_packets().map_err(|e| invalid(e.to_string()))?;
    Ok(None)
}

impl Session {
    /// A new session on its first path.
    pub(crate) fn start(node: &Arc<NodeInner>, mut established: Established) -> io::Result<Arc<Session>> {
        let hello = std::mem::take(&mut established.hello);
        let session = Arc::new(Session {
            peer: established.peer.clone(),
            name: hello["name"].as_str().unwrap_or("").to_owned(),
            id: established.id.clone(),
            features: Mutex::new(hello["features"].clone()),
            node: Arc::downgrade(node),
            closed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            channels: Mutex::default(),
            pending: Mutex::default(),
            // Ours odd if we connected, even if we listened: never the same.
            next_id: AtomicU16::new(if established.dialer { 1 } else { 2 }),
            flow: hello["flow"].as_u64() == Some(1),
            pace: Mutex::new(crate::pace::Pace::new(Instant::now())),
            stream_budget: std::sync::atomic::AtomicUsize::new(STREAM_BUDGET),
            outbox: Mutex::default(),
            reliable: Mutex::new(Reliable { next: 1, ..Default::default() }),
            wake: Wake::new()?,
            path: Mutex::default(),
            arriving: Mutex::default(),
            sealer: std::mem::replace(&mut established.sealer, Sealer::new(&[0; 32], 0)),
            opener: Mutex::new(std::mem::replace(&mut established.opener, Opener::new(&[0; 32], 0))),
            dropped: AtomicU64::new(0),
        });
        // Its first path ready before anyone sees the session.
        let path = prepare(&mut established)?;
        *session.path.lock().unwrap() = Some(path.clone());
        let me = session.clone();
        std::thread::Builder::new().name("link-session".into()).spawn(move || me.run(established, path))?;
        Ok(session)
    }

    /// Another path for this session (both hellos named it): it takes over
    /// if it is better than the one now, if there is none, or if the other
    /// side has none (the one we believe in is dead for them); else closed.
    pub(crate) fn adopt(&self, established: Established) -> bool {
        if self.is_closed() {
            established.close();
            return false;
        }
        let now = self.path.lock().unwrap().clone();
        // Taken if better: a higher rank, or QUIC over TCP at the same one.
        let better = |path: &Path| (rank(&established.kind), established.quic.is_some()) > (rank(&path.kind), path.quic.is_some());
        if !established.forced && now.is_some_and(|path| !path.gone.load(Ordering::SeqCst) && !better(&path)) {
            established.close();
            return false;
        }
        if let Some(before) = self.arriving.lock().unwrap().replace(established) {
            before.close();
        }
        self.wake.wake();
        true
    }

    /// What the other side serves now: {feature: version}.
    pub fn features(&self) -> Value {
        self.features.lock().unwrap().clone()
    }

    /// Ours changed (a feature served or no longer): the other side told.
    pub(crate) fn our_features(&self, features: Value) {
        self.send_message(json!({"t": "features", "features": features}));
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn current(&self) -> Option<Arc<Path>> {
        self.path.lock().unwrap().clone()
    }

    /// What it runs on now: "lan", "usb"; None while it waits for a path.
    pub fn path(&self) -> Option<String> {
        self.current().map(|path| path.kind.clone())
    }

    /// The path is a QUIC connection (quic.rs), not a TCP one or a stream.
    pub fn by_quic(&self) -> bool {
        self.current().is_some_and(|path| path.quic.is_some())
    }

    /// What a QUIC path's congestion controller allows now (bytes a second).
    pub fn rate(&self) -> Option<u64> {
        self.current().and_then(|path| path.quic.as_ref().map(|quic| quic.rate()))
    }

    /// The last round trip over UDP, if it works.
    pub fn rtt(&self) -> Option<Duration> {
        self.current().filter(|path| path.udp_works()).map(|path| Duration::from_micros(path.rtt_us.load(Ordering::Relaxed)))
    }

    /// Bytes sent and not yet on the air (the UDP socket's, the queue's,
    /// the Wi-Fi driver's: each packet counts until it is transmitted) and
    /// the stream's backlog: how far behind the network is.
    pub fn queued(&self) -> usize {
        let mut queued: libc::c_int = 0;
        if let Some(udp) = self.current().as_ref().and_then(|path| path.udp.as_ref()) {
            unsafe { libc::ioctl(udp.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
        }
        queued.max(0) as usize + self.outbox.lock().unwrap().len()
    }

    /// The channels open now.
    pub fn channels(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    /// Input and realtime messages dropped (no room on the way, or no path).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The session over, for every channel at once; the other side told
    /// (a moment for "bye" to go, if there is a path: else it hears
    /// nothing, and gives up after RESUME_WAIT).
    pub fn close(&self) {
        if self.is_closed() || self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.current().is_some() {
            self.send_message(json!({"t": "bye"}));
            let until = Instant::now() + Duration::from_millis(300);
            while !self.outbox.lock().unwrap().is_empty() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(20));  // from rustls to the socket
        }
        self.end("closed here");
    }

    fn end(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        log!("{}: the session is over ({reason})", self.label());
        if let Some(path) = self.current() {
            path.stream.shutdown();
        }
        if let Some(arriving) = self.arriving.lock().unwrap().take() {
            arriving.close();
        }
        self.wake.wake();
        for (_, waiting) in self.pending.lock().unwrap().drain() {
            let _ = waiting.send(Err("the session is over".into()));
        }
        for (_, end) in self.channels.lock().unwrap().drain() {
            sys::shutdown(end.fd.as_raw_fd());
        }
        if let Some(node) = self.node.upgrade() {
            node.gone(self, reason);
        }
    }

    fn label(&self) -> String {
        if self.name.is_empty() { self.peer[..8].to_owned() } else { self.name.clone() }
    }

    // -- the channels ----------------------------------------------------------------

    /// A channel of ours, once the other side takes it.
    pub fn open(self: &Arc<Self>, feature: &str, name: &str, class: Class) -> Result<Channel, String> {
        if self.is_closed() {
            return Err("the session is over".into());
        }
        let (ours, theirs) = sys::seqpacket_pair(CHANNEL_BUFFER).map_err(|e| e.to_string())?;
        // Ours before the other side hears of it: what it sends at once waits in the socket.
        let end = {
            let mut channels = self.channels.lock().unwrap();
            if channels.len() >= MAX_CHANNELS {
                return Err(format!("{feature}/{name}: {MAX_CHANNELS} channels open already"));
            }
            // Ids come round again (u16): none in use, and never the path's own.
            let id = loop {
                let id = self.next_id.fetch_add(2, Ordering::Relaxed);
                if id != DATAGRAM_PATH && !channels.contains_key(&id) {
                    break id;
                }
            };
            let end = Arc::new(End::new(id, class, ours));
            channels.insert(id, end.clone());
            end
        };
        let id = end.id;
        self.inbox(&end);
        let (answer, answered) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, answer);
        self.send_message(json!({"t": "open", "id": id, "feature": feature, "channel": name, "class": class.name()}));
        let result = answered.recv_timeout(OPEN_WAIT).unwrap_or_else(|_| Err("no answer".into()));
        self.pending.lock().unwrap().remove(&id);
        match result {
            Ok(()) => {
                self.pump(end);
                Ok(Channel { fd: theirs, id, class, feature: feature.into(), name: name.into() })
            }
            Err(e) => {
                self.channels.lock().unwrap().remove(&id);
                Err(format!("{feature}/{name}: {e}"))
            }
        }
    }

    /// The other side opens one: taken if a feature of ours serves it.
    fn incoming(self: &Arc<Self>, message: &Value) {
        let id = message["id"].as_u64().unwrap_or(0) as u16;
        let feature = message["feature"].as_str().unwrap_or("").to_owned();
        let name = message["channel"].as_str().unwrap_or("").to_owned();
        let handler = self.node.upgrade().and_then(|node| node.handler(&feature));
        let refuse = |reason: &str| self.send_message(json!({"t": "refuse", "id": id, "reason": reason}));
        let Some(class) = message["class"].as_str().and_then(Class::parse) else { return refuse("unknown class") };
        let Some(handler) = handler else { return refuse("not served") };
        if id == DATAGRAM_PATH || id % 2 == self.next_id.load(Ordering::Relaxed) % 2
            || self.channels.lock().unwrap().contains_key(&id) {
            return refuse("bad id");
        }
        if self.channels.lock().unwrap().len() >= MAX_CHANNELS {
            return refuse("too many channels");
        }
        let (ours, theirs) = match sys::seqpacket_pair(CHANNEL_BUFFER) {
            Ok(pair) => pair,
            Err(e) => return refuse(&e.to_string()),
        };
        let end = Arc::new(End::new(id, class, ours));
        self.inbox(&end);
        self.channels.lock().unwrap().insert(id, end.clone());
        self.send_message(json!({"t": "accept", "id": id}));
        self.pump(end);
        let incoming = Incoming { session: self.clone(), channel: Channel { fd: theirs, id, class, feature, name } };
        let _ = std::thread::Builder::new().name("link-incoming".into()).spawn(move || handler(incoming));
    }

    /// What the feature writes into its end, to the other side, until it
    /// closes it (or the channel or the session ends).
    fn pump(self: &Arc<Self>, end: Arc<End>) {
        let (me, id) = (self.clone(), end.id);
        let spawned = std::thread::Builder::new().name("link-channel".into()).spawn(move || {
            // A control message numbered (8 bytes more) is still a frame.
            let mut buf = vec![0u8; if end.class == Class::Control { frame::MAX - 8 } else { 64 << 10 }];
            let mut sealed = Vec::with_capacity(2048);
            loop {
                let length = match sys::recv(end.fd.as_raw_fd(), &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if me.is_closed() {
                    break;
                }
                if length > buf.len() {
                    log!("{}: channel {} sent a message of {length} bytes: dropped", me.label(), end.id);
                    continue;
                }
                match end.class {
                    Class::Control => {
                        if me.flow && !me.wait_for_credit(&end, length as u64) {
                            break;
                        }
                        me.wait_for_room(end.id);
                        me.queue_reliable(frame::MESSAGE, end.id, &buf[..length])
                    }
                    _ => me.send_datagram(end.id, &buf[..length], &mut sealed),
                }
            }
            me.close_channel(end.id, true);
        });
        if spawned.is_err() {
            self.close_channel(id, true);
        }
    }

    /// Until the other side's window has room for length more bytes on
    /// end (then counted as sent); false if the channel or the session is
    /// over meanwhile.
    fn wait_for_credit(&self, end: &End, length: u64) -> bool {
        loop {
            if self.is_closed() || end.gone.load(Ordering::Relaxed) {
                return false;
            }
            let sent = end.sent.load(Ordering::Relaxed);
            if sent.saturating_add(length) <= end.granted.load(Ordering::Relaxed).saturating_add(WINDOW) {
                end.sent.store(sent + length, Ordering::Relaxed);
                return true;
            }
            std::thread::sleep(ROOM_POLL);
        }
    }

    /// Flow: a control channel's inbox, emptied into its socket on a thread
    /// of its own (blocking there, not the session), telling the other side
    /// what its feature took each GRANT_EVERY.
    fn inbox(self: &Arc<Self>, end: &Arc<End>) {
        if !self.flow || end.class != Class::Control {
            return;
        }
        let (inbox, arriving) = mpsc::channel::<Option<Vec<u8>>>();
        *end.inbox.lock().unwrap() = Some(inbox);
        let (me, end) = (Arc::downgrade(self), end.clone());
        let _ = std::thread::Builder::new().name("link-inbox".into()).spawn(move || {
            let mut told = 0u64;
            while let Ok(Some(message)) = arriving.recv() {
                if sys::send(end.fd.as_raw_fd(), &message, true).is_err() {
                    break;  // its feature gone
                }
                let taken = end.taken.fetch_add(message.len() as u64, Ordering::Relaxed) + message.len() as u64;
                if taken - told >= GRANT_EVERY {
                    told = taken;
                    let Some(me) = me.upgrade() else { break };
                    me.send_message(json!({"t": "credit", "id": end.id, "taken": taken}));
                }
            }
            sys::shutdown(end.fd.as_raw_fd());
        });
    }

    /// Until the channel's own queue is under CONTROL_BACKLOG, or the
    /// session is over (the channels go in turn: one's backlog holds up no
    /// other).
    fn wait_for_room(&self, id: u16) {
        while !self.is_closed() && self.outbox.lock().unwrap().channel_len(id) > CONTROL_BACKLOG {
            std::thread::sleep(ROOM_POLL);
        }
    }

    fn close_channel(&self, id: u16, tell: bool) {
        let Some(end) = self.channels.lock().unwrap().remove(&id) else { return };
        end.gone.store(true, Ordering::Relaxed);
        match end.inbox.lock().unwrap().take() {
            // Closed by the other side: what it sent before goes to the feature first.
            Some(inbox) if !tell => {
                let _ = inbox.send(None);
            }
            _ => sys::shutdown(end.fd.as_raw_fd()),
        }
        if tell && !self.is_closed() {
            self.send_message(json!({"t": "close", "id": id}));
        }
    }

    /// A message for a channel, from the other side, into its socket: a
    /// reliable one for a control channel, a datagram for the others
    /// (count: its counter, if it came sealed), else dropped.
    fn deliver(&self, id: u16, body: &[u8], reliable: bool, count: Option<u64>) {
        let Some(end) = self.channels.lock().unwrap().get(&id).cloned() else { return };
        if (end.class == Class::Control) != reliable {
            return;
        }
        if let (Class::Input, Some(count)) = (end.class, count) {
            // An older state must not undo a newer one.
            if count < end.newest.fetch_max(count.saturating_add(1), Ordering::Relaxed) {
                return;
            }
        }
        let overflowed = match end.inbox.lock().unwrap().as_ref() {
            Some(inbox) => {
                // The other side sends WINDOW past what it was told the
                // feature took, at most: more, and it ignores the flow.
                let received = end.received.fetch_add(body.len() as u64, Ordering::Relaxed) + body.len() as u64;
                let over = received > end.taken.load(Ordering::Relaxed).saturating_add(WINDOW + GRANT_EVERY);
                if !over {
                    let _ = inbox.send(Some(body.to_vec()));
                }
                Some(over)
            }
            None => None,
        };
        match overflowed {
            Some(true) => {
                log!("{}: channel {id} sent past its window: closed", self.label());
                self.close_channel(id, true);
                return;
            }
            Some(false) => return,
            None => {}
        }
        let fd = end.fd.as_raw_fd();
        let mut result = sys::send(fd, body, false);
        if end.class == Class::Control && result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
            && sys::writable(fd, CONTROL_STALL).unwrap_or(false) {
            result = sys::send(fd, body, false);
        }
        match result {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && end.class != Class::Control => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // Its feature gone, or not reading at all (a control message has no room).
                log!("{}: channel {id}: {e}", self.label());
                self.close_channel(id, true);
            }
        }
    }

    // -- the queue -------------------------------------------------------------------

    fn send_message(&self, message: Value) {
        let queue = match message["t"].as_str() {
            // After the channel's own messages, not ahead of them.
            Some("close") => Queue::Channel(message["id"].as_u64().unwrap_or(0) as u16),
            Some("bye") => Queue::Last,
            _ => Queue::Ahead,
        };
        self.outbox.lock().unwrap().push(Waiting { kind: frame::SESSION, channel: 0, body: message.to_string().into_bytes() },
                                         queue);
        self.wake.wake();
    }

    /// A channel's message, queued (numbered as it leaves).
    fn queue_reliable(&self, kind: u8, channel: u16, body: &[u8]) {
        self.outbox.lock().unwrap().push(Waiting { kind, channel, body: body.to_vec() }, Queue::Channel(channel));
        self.wake.wake();
    }

    /// Frames leaving: numbered in the order they leave, kept until the
    /// other side says it has them, onto out.
    fn number(&self, fresh: Vec<Waiting>, out: &mut Vec<u8>) {
        let mut reliable = self.reliable.lock().unwrap();
        for waiting in fresh {
            let number = reliable.next;
            reliable.next += 1;
            let mut numbered = number.to_be_bytes().to_vec();
            numbered.extend_from_slice(&waiting.body);
            let mut bytes = Vec::with_capacity(frame::HEADER + numbered.len());
            frame::encode(waiting.kind, waiting.channel, &numbered, &mut bytes);
            out.extend_from_slice(&bytes);
            reliable.unacked_bytes += bytes.len();
            reliable.unacked.push_back((number, bytes));
        }
    }

    fn queue(&self, kind: u8, channel: u16, body: &[u8]) {
        {
            let mut outbox = self.outbox.lock().unwrap();
            if kind == frame::DATAGRAM && outbox.urgent.len() > self.stream_budget.load(Ordering::Relaxed) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            frame::encode(kind, channel, body, &mut outbox.urgent);
        }
        self.wake.wake();
    }

    // -- the session's thread --------------------------------------------------------

    /// Path after path, until the session is over.
    fn run(self: Arc<Self>, first: Established, path: Arc<Path>) {
        let mut next = Some((first, Some(path)));
        let mut resumed = false;
        while let Some((established, path)) = next.take() {
            let kind = established.kind.clone();
            let reason = self.run_path(established, path, resumed);
            resumed = true;
            if self.is_closed() || self.closing.load(Ordering::SeqCst) {
                return;  // close() ends it
            }
            if let Some(better) = self.arriving.lock().unwrap().take() {
                log!("{}: from {kind} to {}", self.label(), better.kind);
                next = Some((better, None));
                continue;
            }
            log!("{}: the {kind} path is lost ({reason}); waiting for another", self.label());
            *self.path.lock().unwrap() = None;
            if let Some(node) = self.node.upgrade() {
                node.detached(&self);
            }
            let wait = if self.channels() > 0 { RESUME_WAIT_IN_USE } else { RESUME_WAIT };
            let until = Instant::now() + wait;
            while next.is_none() {
                if self.is_closed() {
                    return;
                }
                next = self.arriving.lock().unwrap().take().map(|established| (established, None));
                if next.is_some() {
                    break;
                }
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    self.end(&format!("no path for {} s", wait.as_secs()));
                    return;
                }
                let _ = sys::readable(self.wake.fd(), Some(left.min(Duration::from_millis(500))));
                self.wake.clear();
            }
            if let Some((established, _)) = &next {
                log!("{}: on again, by {}", self.label(), established.kind);
            }
        }
    }

    /// One path, until it is lost, a better one is handed over, or the
    /// session is over: why it ended.
    fn run_path(self: &Arc<Self>, mut established: Established, path: Option<Arc<Path>>, resumed: bool) -> String {
        let path = match path.map(Ok).unwrap_or_else(|| prepare(&mut established)) {
            Ok(path) => path,
            Err(e) => return e.to_string(),
        };
        let Established { mut conn, mut stream, mut decoder, .. } = established;
        if resumed {
            // What the path before did not deliver goes first; datagrams
            // waiting for it are stale now.
            let reliable = self.reliable.lock().unwrap();
            self.outbox.lock().unwrap().restart(reliable.unacked.iter().map(|(_, bytes)| bytes.clone()));
        }
        *self.path.lock().unwrap() = Some(path.clone());
        *self.pace.lock().unwrap() = crate::pace::Pace::new(Instant::now());
        if resumed {
            if let Some(node) = self.node.upgrade() {
                node.moved(self);
            }
        }
        if path.udp.is_some() {
            let (me, path) = (self.clone(), path.clone());
            let _ = std::thread::Builder::new().name("link-datagrams".into()).spawn(move || me.run_datagrams(path));
        }
        if let Some(quic) = &path.quic {
            let (me, path) = (self.clone(), path.clone());
            quic.read_datagrams(move |datagram| {
                let mut buf = datagram.to_vec();
                me.datagram_in(&path, &mut buf, None, &mut Vec::new());
            });
        }
        conn.set_buffer_limit(None);
        let reason = self.drive(&path, &mut conn, &mut stream, &mut decoder).err().unwrap_or_else(|| "a better path".into());
        path.gone.store(true, Ordering::SeqCst);
        path.stream.shutdown();
        if let Some(quic) = &path.quic {
            quic.close();
        }
        reason
    }

    fn drive(self: &Arc<Self>, path: &Arc<Path>, conn: &mut rustls::Connection, stream: &mut Box<dyn Stream>,
             decoder: &mut Decoder) -> Result<(), String> {
        let mut plain = vec![0u8; 64 << 10];
        // The stream's rate while it is full: (window's start, bytes, full in it).
        let mut window = (Instant::now(), 0usize, false);
        self.stream_budget.store(STREAM_BUDGET, Ordering::Relaxed);
        let mut next_ping = Instant::now();
        let mut next_udp_ping = Instant::now();
        // Frames that came with the hello.
        while let Some((kind, channel, body)) = decoder.next().map_err(|e| e.to_string())? {
            self.frame(kind, channel, body)?;
        }
        loop {
            if self.is_closed() {
                return Ok(());
            }
            if self.arriving.lock().unwrap().is_some() {
                return Ok(());  // a better path: the session's thread takes it
            }
            // Out: what was queued, once the last of it left: the urgent
            // first, then a slice of the reliable (Outbox::take).
            let mut paced = None;
            if !conn.wants_write() {
                let reliable = {
                    let mut pace = self.pace.lock().unwrap();
                    let room = self.reliable.lock().unwrap().unacked_bytes < UNACKED_MOST;
                    if room && pace.may_send(Instant::now()) { WRITE_SLICE } else { 0 }
                };
                let (mut out, fresh, taken) = self.outbox.lock().unwrap().take(reliable);
                if !fresh.is_empty() {
                    self.number(fresh, &mut out);
                }
                let mut pace = self.pace.lock().unwrap();
                pace.spent(taken);
                paced = pace.wait();
                if !out.is_empty() {
                    window.1 += out.len();
                    conn.writer().write_all(&out).map_err(|e| e.to_string())?;
                }
            }
            let mut blocked = false;
            while conn.wants_write() {
                match conn.write_tls(stream) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        blocked = true;
                        break;
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            window.2 |= blocked;
            let elapsed = window.0.elapsed();
            if elapsed >= RATE_WINDOW {
                // Full in it: what it took is its rate; else it takes all there is.
                let budget = if window.2 {
                    let rate = window.1 as f64 / elapsed.as_secs_f64();
                    ((rate * REALTIME_WAIT.as_secs_f64()) as usize).clamp(8 << 10, STREAM_BUDGET)
                } else {
                    STREAM_BUDGET
                };
                self.stream_budget.store(budget, Ordering::Relaxed);
                window = (Instant::now(), 0, false);
            }
            // All out, and more queued meanwhile (its wake taken already, while
            // the stream was full): round again at once, after a look at what
            // came, not at the next ping (a transfer went a burst a second).
            let more = !blocked && {
                let outbox = self.outbox.lock().unwrap();
                !outbox.urgent.is_empty() || (outbox.reliable_bytes > 0 && paced.is_none())
            };
            let now = Instant::now();
            let silent = path.us().saturating_sub(path.heard_us.load(Ordering::Relaxed));
            if silent > PATH_SILENCE.as_micros() as u64 {
                return Err(format!("nothing heard for {:.1} s", silent as f64 / 1e6));
            }
            let ack_due = {
                let reliable = self.reliable.lock().unwrap();
                reliable.heard >= reliable.acknowledged.saturating_add(ACK_AFTER)
            };
            if now >= next_ping || ack_due {
                next_ping = now + PING_EVERY;
                let heard = {
                    let mut reliable = self.reliable.lock().unwrap();
                    reliable.acknowledged = reliable.heard;
                    reliable.heard
                };
                self.queue(frame::PING, 0, &heard.to_be_bytes());
                continue;
            }
            if (path.udp.is_some() || path.quic.is_some()) && now >= next_udp_ping {
                let early = path.started.elapsed() < UDP_PROOF;
                let busy = path.realtime_going() && self.outbox.lock().unwrap().reliable_bytes > 0;
                next_udp_ping = now + if busy && path.udp_works() { PROBE_BUSY }
                    else if path.udp_works() || !early { PING_EVERY } else { UDP_PROBE };
                let mut ping = vec![PING];
                ping.extend_from_slice(&path.us().to_be_bytes());
                self.send_datagram_now(path, DATAGRAM_PATH, &ping, &mut Vec::new());
            }
            let datagrams = path.udp.is_some() || path.quic.is_some();
            let mut wait = if datagrams { next_ping.min(next_udp_ping) } else { next_ping }.saturating_duration_since(now);
            if let Some(paced) = paced.filter(|_| self.outbox.lock().unwrap().reliable_bytes > 0) {
                wait = wait.min(paced);  // the reliable frames' next turn
            }
            let mut fds = [
                libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN | if blocked { libc::POLLOUT } else { 0 }, revents: 0 },
                libc::pollfd { fd: self.wake.fd(), events: libc::POLLIN, revents: 0 },
            ];
            sys::poll(&mut fds, Some(if more { Duration::ZERO } else { wait.min(PING_EVERY) })).map_err(|e| e.to_string())?;
            if fds[1].revents != 0 {
                self.wake.clear();
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
                continue;
            }
            loop {
                match conn.read_tls(stream) {
                    Ok(0) => return Err("the other side closed".into()),
                    Ok(_) => {
                        conn.process_new_packets().map_err(|e| e.to_string())?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.to_string()),
                }
                // Out at once: rustls holds only so much read and not taken.
                loop {
                    match conn.reader().read(&mut plain) {
                        Ok(0) => return Err("the other side closed".into()),
                        Ok(n) => decoder.push(&plain[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e.to_string()),
                    }
                }
            }
            while let Some((kind, channel, body)) = decoder.next().map_err(|e| e.to_string())? {
                path.heard();
                self.frame(kind, channel, body)?;
            }
        }
    }

    /// A frame from the stream; an error ends the path (the other side
    /// broke the protocol).
    fn frame(self: &Arc<Self>, kind: u8, channel: u16, body: Vec<u8>) -> Result<(), String> {
        match kind {
            frame::SESSION | frame::MESSAGE => {
                if body.len() < 8 {
                    return Err("a reliable frame with no number".into());
                }
                let number = u64::from_be_bytes(body[..8].try_into().unwrap());
                {
                    let mut reliable = self.reliable.lock().unwrap();
                    if number <= reliable.heard {
                        return Ok(());  // had it: sent again after a path was lost
                    }
                    // A stream keeps them in order, and none is skipped.
                    if number != reliable.heard + 1 {
                        return Err(format!("frame {number} after {}", reliable.heard));
                    }
                    reliable.heard = number;
                }
                if kind == frame::MESSAGE {
                    self.deliver(channel, &body[8..], true, None);
                } else {
                    match serde_json::from_slice::<Value>(&body[8..]) {
                        Ok(message) => self.message(&message),
                        Err(e) => log!("{}: a bad message: {e}", self.label()),
                    }
                }
            }
            frame::DATAGRAM => self.deliver(channel, &body, false, None),
            frame::PING if body.len() >= 8 => {
                let acknowledged = u64::from_be_bytes(body[..8].try_into().unwrap());
                let mut reliable = self.reliable.lock().unwrap();
                while reliable.unacked.front().is_some_and(|(number, _)| *number <= acknowledged) {
                    if let Some((_, bytes)) = reliable.unacked.pop_front() {
                        reliable.unacked_bytes -= bytes.len();
                    }
                }
            }
            _ => log!("{}: a frame of kind {kind}", self.label()),
        }
        Ok(())
    }

    fn message(self: &Arc<Self>, message: &Value) {
        let id = message["id"].as_u64().unwrap_or(0) as u16;
        match message["t"].as_str().unwrap_or("") {
            "open" => self.incoming(message),
            "accept" | "refuse" => {
                if let Some(waiting) = self.pending.lock().unwrap().remove(&id) {
                    let result = if message["t"] == "accept" {
                        Ok(())
                    } else {
                        Err(message["reason"].as_str().unwrap_or("refused").to_owned())
                    };
                    let _ = waiting.send(result);
                }
            }
            "close" => self.close_channel(id, false),
            "bye" => self.end("closed by the other side"),
            "features" => *self.features.lock().unwrap() = message["features"].clone(),
            "credit" => {
                if let (Some(end), Some(taken)) = (self.channels.lock().unwrap().get(&id), message["taken"].as_u64()) {
                    end.granted.fetch_max(taken, Ordering::Relaxed);
                }
            }
            other => log!("{}: a message of type {other:?}", self.label()),
        }
    }

    // -- the datagrams ---------------------------------------------------------------

    /// Input and realtime: a datagram once UDP is proven, else over the
    /// stream; no path: lost.
    fn send_datagram(&self, channel: u16, body: &[u8], sealed: &mut Vec<u8>) {
        if body.len() > MAX_DATAGRAM {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let current = self.current();
        if let Some(path) = &current {
            path.realtime_us.store(path.us().max(1), Ordering::Relaxed);
        }
        match current {
            Some(path) if path.udp_works() => self.send_datagram_now(&path, channel, body, sealed),
            Some(_) => self.queue(frame::DATAGRAM, channel, body),
            None => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn send_datagram_now(&self, path: &Path, channel: u16, body: &[u8], sealed: &mut Vec<u8>) {
        if let Some(quic) = &path.quic {
            self.sealer.seal(channel, body, sealed);
            if !quic.send(sealed) {
                self.dropped.fetch_add(1, Ordering::Relaxed);  // late, or too big
            }
            return;
        }
        let (Some(udp), Some(to)) = (&path.udp, *path.udp_peer.lock().unwrap()) else { return };
        self.sealer.seal(channel, body, sealed);
        let mut result = sys::send_to(udp.as_raw_fd(), sealed, &to);
        // No room: a moment for some (a keyframe's tail), then lost.
        if result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
            && sys::writable(udp.as_raw_fd(), UDP_STALL).unwrap_or(false) {
            result = sys::send_to(udp.as_raw_fd(), sealed, &to);
        }
        if result.is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn run_datagrams(self: Arc<Self>, path: Arc<Path>) {
        let Some(udp) = &path.udp else { return };
        let mut buf = vec![0u8; 2048];
        let mut sealed = Vec::with_capacity(64);
        while !self.is_closed() && !path.gone.load(Ordering::SeqCst) {
            let (length, from) = match udp.recv_from(&mut buf) {
                Ok(got) => got,
                Err(_) => continue,  // the timeout: look at closed again
            };
            self.datagram_in(&path, &mut buf[..length], Some(from), &mut sealed);
        }
    }

    /// A datagram that came on path (from: where, for UDP): its channel's,
    /// or a ping answered, or a pong's round trip taken.
    fn datagram_in(&self, path: &Path, buf: &mut [u8], from: Option<SocketAddr>, sealed: &mut Vec<u8>) {
        let Some((count, channel, body)) = self.opener.lock().unwrap().open(buf) else { return };
        path.heard();
        // Where it came from is where the other side is now (its address
        // may change; only a sealed packet can say so).
        if let Some(from) = from {
            let mut peer = path.udp_peer.lock().unwrap();
            if *peer != Some(from) {
                *peer = Some(from);
            }
        }
        if channel != DATAGRAM_PATH {
            self.deliver(channel, body, false, Some(count));
        } else if body.len() == 9 && body[0] == PING {
            let mut pong = body.to_vec();
            pong[0] = PONG;
            self.send_datagram_now(path, DATAGRAM_PATH, &pong, sealed);
        } else if body.len() == 9 && body[0] == PONG {
            let sent = u64::from_be_bytes(body[1..9].try_into().unwrap());
            let now = path.us();
            path.rtt_us.store(now.saturating_sub(sent), Ordering::Relaxed);
            path.proven_us.store(now.max(1), Ordering::Relaxed);
            // A QUIC path is paced by its own controller (cc.rs); pace.rs
            // paces a TCP one.
            if path.quic.is_none() {
                self.pace.lock().unwrap().sample(now.saturating_sub(sent), path.realtime_going(), Instant::now());
            }
        }
    }
}
