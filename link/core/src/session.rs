//! A session with a paired device: one TLS 1.3 stream (the session's own
//! messages and the control channels), one UDP socket (input and realtime,
//! sealed with keys from the TLS exporter), and the channels.
//!
//! Threads, none of them busy while nothing moves:
//! - the stream's: it alone touches the TLS connection; reads, writes what
//!   the others queued (woken by an eventfd), pings each second, and ends
//!   the session after SILENCE without a word from the other side;
//! - the datagrams': opens what arrives and hands it to its channel;
//! - one per channel: what the feature writes, to the other side.
//!
//! Input and realtime go as datagrams once a ping has come back over UDP
//! (both directions work), else over the stream: a network that drops UDP
//! still works, only less well.

use crate::frame::{self, Decoder};
use crate::node::NodeInner;
use crate::seal::{Opener, Sealer};
use crate::sys::{self, Wake};
use crate::{log, Class};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use crate::stream::Stream;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A ping each this long, on the stream and over UDP.
pub const PING_EVERY: Duration = Duration::from_secs(1);
/// Nothing from the other side this long: the session is over.
pub const SILENCE: Duration = Duration::from_millis(3500);
/// A pong this recent over UDP: datagrams go both ways.
const UDP_PROOF: Duration = Duration::from_secs(3);
/// While UDP is not proven yet, a ping this often (the first ones sooner).
const UDP_PROBE: Duration = Duration::from_millis(200);
const HANDSHAKE: Duration = Duration::from_secs(5);
const OPEN_WAIT: Duration = Duration::from_secs(5);
/// An input or realtime message's largest size: with the seal (30 bytes),
/// UDP and IPv6 (48), a datagram of 1500 bytes, which any network carries.
pub const MAX_DATAGRAM: usize = 1422;
/// Realtime over the stream: dropped while this much waits to be sent.
const STREAM_BUDGET: usize = 256 << 10;
/// Room in the UDP socket, each way: a keyframe's packets at once.
const UDP_BUFFER: libc::c_int = 4 << 20;
/// Room in a channel's socket, each way (the system caps it: a control
/// message is at most about 200 KB; more goes on a bulk channel).
const CHANNEL_BUFFER: usize = 1 << 20;
/// A control message with no room in its channel: this long for the
/// feature to read, then the channel is closed (it holds up the stream).
const CONTROL_STALL: Duration = Duration::from_secs(1);
const EXPORTER_LABEL: &[u8] = b"EXPORTER-plus-one-link datagrams";
const PATH: u16 = 0;  // the datagrams' own channel: pings and pongs
const PING: u8 = 1;
const PONG: u8 = 2;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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
}

pub struct Session {
    /// The other side's certificate's fingerprint, and what its hello said.
    pub peer: String,
    pub name: String,
    /// What it runs on: "lan", "usb", "bluetooth".
    pub path: String,
    features: Mutex<Value>,
    node: Weak<NodeInner>,
    started: Instant,
    closed: AtomicBool,
    channels: Mutex<HashMap<u16, Arc<End>>>,
    pending: Mutex<HashMap<u16, mpsc::Sender<Result<(), String>>>>,
    next_id: AtomicU16,
    outbox: Mutex<Vec<u8>>,
    wake: Wake,
    stream: Box<dyn Stream>,
    udp: Option<UdpSocket>,
    udp_peer: Mutex<Option<SocketAddr>>,
    sealer: Sealer,
    proven_us: AtomicU64,
    rtt_us: AtomicU64,
    dropped: AtomicU64,
}

/// What the TLS handshake and the hellos left: the session starts from it.
pub(crate) struct Established {
    conn: rustls::Connection,
    stream: Box<dyn Stream>,
    decoder: Decoder,
    peer: String,
    hello: Value,
    udp: Option<UdpSocket>,
    udp_peer: Option<SocketAddr>,
    sealer: Sealer,
    opener: Opener,
    dialer: bool,
    path: String,
}

/// The TLS handshake, the keys and the hellos, on a new connection. The
/// verifier has checked the other side is paired. path: what it runs on.
pub(crate) fn establish(mut conn: rustls::Connection, mut stream: Box<dyn Stream>, dialer: bool, hello: Value,
                        path: &str) -> io::Result<Established> {
    stream.set_timeouts(Some(HANDSHAKE))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut stream)?;
    }
    let peer = conn.peer_certificates().and_then(|c| c.first()).map(|c| crate::fingerprint(c))
        .ok_or_else(|| invalid("no certificate"))?;
    let keys = conn.export_keying_material([0u8; 64], EXPORTER_LABEL, None).map_err(|e| invalid(e.to_string()))?;
    let (ours, theirs) = if dialer { (0, 32) } else { (32, 0) };
    let (send_direction, receive_direction) =
        if dialer { (crate::seal::DIALER, crate::seal::LISTENER) } else { (crate::seal::LISTENER, crate::seal::DIALER) };
    let sealer = Sealer::new(keys[ours..ours + 32].try_into().unwrap(), send_direction);
    let opener = Opener::new(keys[theirs..theirs + 32].try_into().unwrap(), receive_direction);

    // Datagrams as UDP when there is IP under the stream.
    let udp = match stream.addresses() {
        Some((_, local)) => {
            let udp = UdpSocket::bind(SocketAddr::new(local, 0))?;
            // As much as the system allows (net.core.wmem_max, rmem_max).
            sys::set_int(udp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, UDP_BUFFER);
            sys::set_int(udp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, UDP_BUFFER);
            Some(udp)
        }
        None => None,
    };
    let mut hello = hello;
    if let Some(udp) = &udp {
        hello["udp"] = json!(udp.local_addr()?.port());
    }
    let mut bytes = Vec::new();
    frame::encode(frame::SESSION, 0, hello.to_string().as_bytes(), &mut bytes);
    conn.writer().write_all(&bytes)?;
    while conn.wants_write() {
        conn.write_tls(&mut stream)?;
    }
    let mut decoder = Decoder::default();
    let theirs = loop {
        if let Some((kind, _, body)) = read_frame(&mut conn, &mut stream, &mut decoder)? {
            if kind != frame::SESSION {
                return Err(invalid("a frame before the hello"));
            }
            let message: Value = serde_json::from_slice(&body).map_err(|e| invalid(e.to_string()))?;
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
    Ok(Established { conn, stream, decoder, peer, hello: theirs, udp, udp_peer, sealer, opener, dialer,
                     path: path.to_owned() })
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
    pub(crate) fn start(node: &Arc<NodeInner>, established: Established) -> io::Result<Arc<Session>> {
        let Established { conn, stream, decoder, peer, hello, udp, udp_peer, sealer, opener, dialer, path } = established;
        stream.set_timeouts(None)?;
        stream.set_nonblocking(true)?;
        if let Some(udp) = &udp {
            udp.set_read_timeout(Some(Duration::from_millis(500)))?;
        }
        let session = Arc::new(Session {
            peer,
            name: hello["name"].as_str().unwrap_or("").to_owned(),
            path,
            features: Mutex::new(hello["features"].clone()),
            node: Arc::downgrade(node),
            started: Instant::now(),
            closed: AtomicBool::new(false),
            channels: Mutex::default(),
            pending: Mutex::default(),
            // Ours odd if we connected, even if we listened: never the same.
            next_id: AtomicU16::new(if dialer { 1 } else { 2 }),
            outbox: Mutex::default(),
            wake: Wake::new()?,
            stream: stream.try_clone()?,
            udp,
            udp_peer: Mutex::new(udp_peer),
            sealer,
            proven_us: AtomicU64::new(0),
            rtt_us: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let me = session.clone();
        std::thread::Builder::new().name("link-stream".into()).spawn(move || {
            let reason = me.run_stream(conn, stream, decoder).err().unwrap_or_else(|| "closed".into());
            me.end(&reason);
        })?;
        if session.udp.is_some() {
            let me = session.clone();
            std::thread::Builder::new().name("link-datagrams".into()).spawn(move || me.run_datagrams(opener))?;
        }
        Ok(session)
    }

    fn us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
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

    /// The last round trip over UDP, if it works.
    pub fn rtt(&self) -> Option<Duration> {
        self.udp_works().then(|| Duration::from_micros(self.rtt_us.load(Ordering::Relaxed)))
    }

    /// Bytes sent and not yet on the air (the UDP socket's, the queue's,
    /// the Wi-Fi driver's: each packet counts until it is transmitted) and
    /// the stream's backlog: how far behind the network is.
    pub fn queued(&self) -> usize {
        let mut queued: libc::c_int = 0;
        if let Some(udp) = &self.udp {
            unsafe { libc::ioctl(udp.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
        }
        queued.max(0) as usize + self.outbox.lock().unwrap().len()
    }

    /// The channels open now.
    pub fn channels(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    /// Input and realtime messages dropped (no room on the way).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn udp_works(&self) -> bool {
        let proven = self.proven_us.load(Ordering::Relaxed);
        proven != 0 && self.us().saturating_sub(proven) < UDP_PROOF.as_micros() as u64
    }

    /// The session over, for every channel at once; the other side told
    /// (as far as the stream still goes).
    pub fn close(&self) {
        self.end("closed here");
    }

    fn end(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        log!("{}: the session is over ({reason})", self.label());
        self.stream.shutdown();
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
        let id = self.next_id.fetch_add(2, Ordering::Relaxed);
        let (ours, theirs) = sys::seqpacket_pair(CHANNEL_BUFFER).map_err(|e| e.to_string())?;
        // Ours before the other side hears of it: what it sends at once waits in the socket.
        let end = Arc::new(End { id, class, fd: ours, newest: AtomicU64::new(0) });
        self.channels.lock().unwrap().insert(id, end.clone());
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
        if id == PATH || id % 2 == self.next_id.load(Ordering::Relaxed) % 2 || self.channels.lock().unwrap().contains_key(&id) {
            return refuse("bad id");
        }
        let (ours, theirs) = match sys::seqpacket_pair(CHANNEL_BUFFER) {
            Ok(pair) => pair,
            Err(e) => return refuse(&e.to_string()),
        };
        let end = Arc::new(End { id, class, fd: ours, newest: AtomicU64::new(0) });
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
            let mut buf = vec![0u8; if end.class == Class::Control { frame::MAX } else { 64 << 10 }];
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
                    Class::Control => me.queue(frame::MESSAGE, end.id, &buf[..length]),
                    _ => me.send_datagram(end.id, &buf[..length], &mut sealed),
                }
            }
            me.close_channel(end.id, true);
        });
        if spawned.is_err() {
            self.close_channel(id, true);
        }
    }

    fn close_channel(&self, id: u16, tell: bool) {
        let Some(end) = self.channels.lock().unwrap().remove(&id) else { return };
        sys::shutdown(end.fd.as_raw_fd());
        if tell && !self.is_closed() {
            self.send_message(json!({"t": "close", "id": id}));
        }
    }

    /// A message for a channel, from the other side, into its socket.
    fn deliver(&self, id: u16, body: &[u8], count: Option<u64>) {
        let Some(end) = self.channels.lock().unwrap().get(&id).cloned() else { return };
        if let (Class::Input, Some(count)) = (end.class, count) {
            // An older state must not undo a newer one.
            if count < end.newest.fetch_max(count + 1, Ordering::Relaxed) {
                return;
            }
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

    // -- the stream ------------------------------------------------------------------

    fn send_message(&self, message: Value) {
        self.queue(frame::SESSION, 0, message.to_string().as_bytes());
    }

    fn queue(&self, kind: u8, channel: u16, body: &[u8]) {
        {
            let mut outbox = self.outbox.lock().unwrap();
            if kind == frame::DATAGRAM && outbox.len() > STREAM_BUDGET {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            frame::encode(kind, channel, body, &mut outbox);
        }
        self.wake.wake();
    }

    fn run_stream(self: &Arc<Self>, mut conn: rustls::Connection, mut stream: Box<dyn Stream>, mut decoder: Decoder)
                  -> Result<(), String> {
        conn.set_buffer_limit(None);
        let mut plain = vec![0u8; 64 << 10];
        let mut heard = Instant::now();
        let mut next_ping = Instant::now();
        let mut next_udp_ping = Instant::now();
        // Frames that came with the hello.
        while let Some((kind, channel, body)) = decoder.next().map_err(|e| e.to_string())? {
            self.frame(kind, channel, body);
        }
        loop {
            if self.is_closed() {
                return Ok(());
            }
            // Out: what was queued, once the last of it left (the queue's
            // length is what realtime's budget looks at).
            if !conn.wants_write() {
                let out = std::mem::take(&mut *self.outbox.lock().unwrap());
                if !out.is_empty() {
                    conn.writer().write_all(&out).map_err(|e| e.to_string())?;
                }
            }
            let mut blocked = false;
            while conn.wants_write() {
                match conn.write_tls(&mut stream) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        blocked = true;
                        break;
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            let now = Instant::now();
            if now.duration_since(heard) > SILENCE {
                return Err(format!("nothing heard for {:.1} s", SILENCE.as_secs_f64()));
            }
            if now >= next_ping {
                next_ping = now + PING_EVERY;
                self.send_message(json!({"t": "ping"}));
                continue;
            }
            if self.udp.is_some() && now >= next_udp_ping {
                let early = self.started.elapsed() < UDP_PROOF;
                next_udp_ping = now + if self.udp_works() || !early { PING_EVERY } else { UDP_PROBE };
                let mut ping = vec![PING];
                ping.extend_from_slice(&self.us().to_be_bytes());
                self.send_datagram_now(PATH, &ping, &mut Vec::new());
            }
            let wait = if self.udp.is_some() { next_ping.min(next_udp_ping) } else { next_ping }.saturating_duration_since(now);
            let mut fds = [
                libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN | if blocked { libc::POLLOUT } else { 0 }, revents: 0 },
                libc::pollfd { fd: self.wake.fd(), events: libc::POLLIN, revents: 0 },
            ];
            sys::poll(&mut fds, Some(wait)).map_err(|e| e.to_string())?;
            if fds[1].revents != 0 {
                self.wake.clear();
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
                continue;
            }
            loop {
                match conn.read_tls(&mut stream) {
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
                heard = Instant::now();
                self.frame(kind, channel, body);
            }
        }
    }

    fn frame(self: &Arc<Self>, kind: u8, channel: u16, body: Vec<u8>) {
        match kind {
            frame::SESSION => match serde_json::from_slice::<Value>(&body) {
                Ok(message) => self.message(&message),
                Err(e) => log!("{}: a bad message: {e}", self.label()),
            },
            frame::MESSAGE | frame::DATAGRAM => self.deliver(channel, &body, None),
            _ => log!("{}: a frame of kind {kind}", self.label()),
        }
    }

    fn message(self: &Arc<Self>, message: &Value) {
        let id = message["id"].as_u64().unwrap_or(0) as u16;
        match message["t"].as_str().unwrap_or("") {
            "ping" => {}
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
            "features" => *self.features.lock().unwrap() = message["features"].clone(),
            other => log!("{}: a message of type {other:?}", self.label()),
        }
    }

    // -- the datagrams ---------------------------------------------------------------

    /// Input and realtime: a datagram once UDP is proven, else over the stream.
    fn send_datagram(&self, channel: u16, body: &[u8], sealed: &mut Vec<u8>) {
        if body.len() > MAX_DATAGRAM {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.udp_works() {
            self.send_datagram_now(channel, body, sealed);
        } else {
            self.queue(frame::DATAGRAM, channel, body);
        }
    }

    fn send_datagram_now(&self, channel: u16, body: &[u8], sealed: &mut Vec<u8>) {
        let (Some(udp), Some(to)) = (&self.udp, *self.udp_peer.lock().unwrap()) else { return };
        self.sealer.seal(channel, body, sealed);
        if let Err(e) = sys::send_to(udp.as_raw_fd(), sealed, &to) {
            if e.kind() == io::ErrorKind::WouldBlock {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn run_datagrams(self: Arc<Self>, mut opener: Opener) {
        let Some(udp) = &self.udp else { return };
        let mut buf = vec![0u8; 2048];
        let mut sealed = Vec::with_capacity(64);
        while !self.is_closed() {
            let (length, from) = match udp.recv_from(&mut buf) {
                Ok(got) => got,
                Err(_) => continue,  // the timeout: look at closed again
            };
            let Some((count, channel, body)) = opener.open(&mut buf[..length]) else { continue };
            // Where it came from is where the other side is now (its
            // address may change; only a sealed packet can say so).
            {
                let mut peer = self.udp_peer.lock().unwrap();
                if *peer != Some(from) {
                    *peer = Some(from);
                }
            }
            if channel != PATH {
                self.deliver(channel, body, Some(count));
            } else if body.len() == 9 && body[0] == PING {
                let mut pong = body.to_vec();
                pong[0] = PONG;
                self.send_datagram_now(PATH, &pong, &mut sealed);
            } else if body.len() == 9 && body[0] == PONG {
                let sent = u64::from_be_bytes(body[1..9].try_into().unwrap());
                let now = self.us();
                self.rtt_us.store(now.saturating_sub(sent), Ordering::Relaxed);
                self.proven_us.store(now.max(1), Ordering::Relaxed);
            }
        }
    }
}

