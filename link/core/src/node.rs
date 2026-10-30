//! This side of the link: who we are, what we serve, our sessions (one per
//! paired device). It listens, connects, and tells who comes and goes and
//! which way. A connection from a device we have a session with becomes
//! that session's path (both hellos name it), not a new session.

use crate::identity::{Identity, Paired};
use crate::session::{self, Incoming, PathInfo, Session};
use crate::stream::Stream;
use crate::log;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A feature's code for the channels the other side opens for it.
pub type Handler = Arc<dyn Fn(Incoming) + Send + Sync>;

/// The other side says it takes QUIC (its hello): a QUIC path to it is
/// tried this long, beside the TCP one the session has already.
const QUIC_TRY: Duration = Duration::from_secs(3);

pub enum Event {
    /// A session with a paired device, up.
    Up(Arc<Session>),
    /// Its path lost: it waits session::RESUME_WAIT for another (the
    /// dialer's to make).
    Detached(Arc<Session>),
    /// On another path now (session.path()).
    Moved(Arc<Session>),
    /// Its path's budget moved (architecture §3.11; bytes a second): what
    /// realtime and bulk may use now, as a QUIC path's controller allows,
    /// or a Bluetooth path's stream drains (told as it begins too).
    Budget(Arc<Session>, u64),
    /// One of its paths changed state (session::PathState): taken (active,
    /// with Up or Moved), suspect, active again, failed (before Detached,
    /// unless another takes over).
    Path(Arc<Session>, PathInfo),
    /// A device's session over: (its fingerprint, why).
    Gone(String, String),
}

pub(crate) struct NodeInner {
    name: String,
    identity_fingerprint: String,  // ours (a relay's rendezvous is made of the two)
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    paired: Paired,
    served: Mutex<HashMap<String, (u32, Handler)>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    events: Mutex<Option<Arc<dyn Fn(Event) + Send + Sync>>>,
    listeners: Mutex<Vec<TcpListener>>,
    /// Who may start a handshake, by TCP or QUIC (gate.rs).
    gate: Arc<crate::gate::Gate>,
    /// QUIC's endpoint: the listening one, or one to dial from.
    quic: Mutex<Option<quinn::Endpoint>>,
    /// Where we take QUIC (0: we do not), said in our hello.
    quic_port: std::sync::atomic::AtomicU16,
    /// Our TCP paths to a device that takes QUIC move to it (upgrade_to_quic).
    quic_upgrade: AtomicBool,
    closed: AtomicBool,
}

impl NodeInner {
    pub(crate) fn handler(&self, feature: &str) -> Option<Handler> {
        self.served.lock().unwrap().get(feature).map(|(_, handler)| handler.clone())
    }

    fn features(&self) -> Value {
        let features: Map<String, Value> =
            self.served.lock().unwrap().iter().map(|(name, (version, _))| (name.clone(), json!(version))).collect();
        Value::Object(features)
    }

    fn hello(&self) -> Value {
        // flow: control channels flow-controlled (session.rs, WINDOW);
        // quic: the UDP port we take QUIC on, if we do (quic.rs).
        let mut hello = json!({"t": "hello", "name": self.name, "features": self.features(), "flow": 1});
        let port = self.quic_port.load(Ordering::SeqCst);
        if port != 0 {
            hello["quic"] = json!(port);
        }
        hello
    }

    /// The session we have with peer, its id (a new path goes on with it),
    /// and whether it has no path now.
    fn ours(&self, peer: &str) -> Option<(String, bool)> {
        self.sessions.lock().unwrap().get(peer).filter(|s| !s.is_closed()).map(|s| (s.id.clone(), s.path().is_none()))
    }

    fn features_changed(&self) {
        let features = self.features();
        for session in self.sessions.lock().unwrap().values() {
            session.our_features(features.clone());
        }
    }

    fn event(&self, event: Event) {
        let listener = self.events.lock().unwrap().clone();
        if let Some(listener) = listener {
            listener(event);
        }
    }

    /// A session's end, from the session itself.
    pub(crate) fn gone(&self, session: &Session, reason: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.get(&session.peer).is_some_and(|s| std::ptr::eq(Arc::as_ptr(s), session)) {
            sessions.remove(&session.peer);
            drop(sessions);
            self.event(Event::Gone(session.peer.clone(), reason.to_owned()));
        }
    }

    pub(crate) fn detached(&self, session: &Arc<Session>) {
        self.event(Event::Detached(session.clone()));
    }

    pub(crate) fn moved(&self, session: &Arc<Session>) {
        self.event(Event::Moved(session.clone()));
    }

    pub(crate) fn budget(&self, session: &Arc<Session>, rate: u64) {
        self.event(Event::Budget(session.clone(), rate));
    }

    pub(crate) fn path(&self, session: &Arc<Session>, path: PathInfo) {
        self.event(Event::Path(session.clone(), path));
    }

    fn accept(self: &Arc<Self>, stream: Box<dyn Stream>, kind: &str) -> std::io::Result<Arc<Session>> {
        let conn = rustls::ServerConnection::new(self.server.clone()).map_err(|e| std::io::Error::other(e.to_string()))?;
        let established = session::establish(conn.into(), stream, false, self.hello(), kind, &|peer| self.ours(peer))?;
        self.add(established)
    }

    /// As accept(), on a QUIC connection's stream (its datagrams the path's).
    fn accept_quic(self: &Arc<Self>, stream: Box<dyn Stream>, quic: crate::quic::Quic) -> std::io::Result<Arc<Session>> {
        let conn = rustls::ServerConnection::new(self.server.clone()).map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut established = match session::establish(conn.into(), stream, false, self.hello(), "lan", &|peer| self.ours(peer)) {
            Ok(established) => established,
            Err(e) => {
                quic.close();
                return Err(e);
            }
        };
        established.set_quic(quic);
        self.add(established)
    }

    fn connect_quic(self: &Arc<Self>, address: SocketAddr, timeout: Duration, kind: &str) -> Result<Arc<Session>, String> {
        let endpoint = self.quic_endpoint().map_err(|e| e.to_string())?;
        let (stream, quic) = crate::quic::dial(&endpoint, &self.client, address, timeout)?;
        let name = rustls::pki_types::ServerName::try_from("plus-one.invalid").unwrap();
        let conn = rustls::ClientConnection::new(self.client.clone(), name).map_err(|e| e.to_string())?;
        let mut established = match session::establish(conn.into(), Box::new(stream), true, self.hello(), kind,
                                                       &|peer| self.ours(peer)) {
            Ok(established) => established,
            Err(e) => {
                quic.close();
                return Err(e.to_string());
            }
        };
        established.set_quic(quic);
        self.add(established).map_err(|e| e.to_string())
    }

    /// The endpoint QUIC dials from: the listening one, else one of its own.
    fn quic_endpoint(&self) -> std::io::Result<quinn::Endpoint> {
        let mut quic = self.quic.lock().unwrap();
        if let Some(endpoint) = quic.as_ref() {
            return Ok(endpoint.clone());
        }
        let endpoint = crate::quic::endpoint(([0, 0, 0, 0], 0).into(), None)?;
        *quic = Some(endpoint.clone());
        Ok(endpoint)
    }

    /// A connection made a session: a path for the one we have with the
    /// device (both hellos named it), or a new session, and the one
    /// before, if any, ends (the device forgot it: restarted, say).
    fn add(self: &Arc<Self>, established: session::Established) -> std::io::Result<Arc<Session>> {
        // A TCP path we made to a device that takes QUIC: a QUIC path to
        // it after, which takes over (QUIC over TCP, the same kind); only
        // from a path taken, not from one dropped for a better.
        let upgrade = established.quic_offer().filter(|_| self.quic_upgrade.load(Ordering::SeqCst))
            .map(|address| (address, established.kind().to_owned()));
        if let Some(id) = established.resumes.clone() {
            let existing = self.sessions.lock().unwrap().get(&established.peer).cloned();
            if let Some(existing) = existing.filter(|s| s.id == id) {
                let kind = established.kind().to_owned();
                if existing.adopt(established) {
                    self.upgrade(&existing, upgrade);
                } else {
                    log!("{}: a {kind} path not taken (a better one is up)", existing.name);
                }
                return Ok(existing);
            }
        }
        let session = Session::start(self, established)?;
        let (before, over) = {
            let mut sessions = self.sessions.lock().unwrap();
            // Made while the node closed (close() saw none of it): not kept.
            if self.closed.load(Ordering::SeqCst) {
                drop(sessions);
                session.close();
                return Err(std::io::Error::other("the node is closed"));
            }
            // Over already (the other side's bye right behind its hello):
            // its end found it nowhere to be taken out of. Not kept, and
            // the one before ends all the same (the device has no other).
            if session.is_closed() {
                (sessions.remove(&session.peer), true)
            } else {
                (sessions.insert(session.peer.clone(), session.clone()), false)
            }
        };
        if over {
            if let Some(before) = before {
                before.close();  // its end finds it nowhere either: Gone told here
            }
            self.event(Event::Gone(session.peer.clone(), "over as it began".into()));
            return Err(std::io::Error::other("the session was over as it began"));
        }
        // Served since the hello went (the session was not among them yet).
        session.our_features(self.features());
        if let Some(before) = before {
            before.close();
        }
        log!("{}: a session ({})", if session.name.is_empty() { &session.peer[..8] } else { &session.name },
             session.peer.get(..16).unwrap_or(""));
        self.event(Event::Up(session.clone()));
        self.upgrade(&session, upgrade);
        Ok(session)
    }

    /// The QUIC path add() asks for, on a thread: unless the session is on
    /// QUIC already by then (one dial, one QUIC path), or over.
    fn upgrade(self: &Arc<Self>, session: &Arc<Session>, upgrade: Option<(SocketAddr, String)>) {
        let Some((address, kind)) = upgrade else { return };
        let (node, session) = (self.clone(), session.clone());
        let _ = std::thread::Builder::new().name("link-quic-upgrade".into()).spawn(move || {
            if session.is_closed() || session.by_quic() {
                return;
            }
            if let Err(e) = node.connect_quic(address, QUIC_TRY, &kind) {
                log!("{address}: no QUIC path ({e}): TCP stays");
            }
        });
    }
}

#[derive(Clone)]
pub struct Node(Arc<NodeInner>);

impl Node {
    /// name: ours, as the other side shows it.
    pub fn new(name: &str, identity: &Identity, paired: Paired) -> Result<Node, String> {
        Ok(Node(Arc::new(NodeInner {
            name: name.to_owned(),
            identity_fingerprint: identity.fingerprint(),
            client: identity.client_config(paired.clone())?,
            server: identity.server_config(paired.clone())?,
            paired,
            served: Mutex::default(),
            sessions: Mutex::default(),
            events: Mutex::default(),
            listeners: Mutex::default(),
            gate: crate::gate::Gate::new(),
            quic: Mutex::default(),
            quic_port: std::sync::atomic::AtomicU16::new(0),
            quic_upgrade: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        })))
    }

    /// A feature of ours, its version (the hellos say it, and the sessions
    /// up are told): the other side's channels for it go to handler, each
    /// on a thread of its own.
    pub fn serve(&self, feature: &str, version: u32, handler: impl Fn(Incoming) + Send + Sync + 'static) {
        self.0.served.lock().unwrap().insert(feature.to_owned(), (version, Arc::new(handler)));
        self.0.features_changed();
    }

    /// A feature no longer served: its channels from then on refused.
    pub fn unserve(&self, feature: &str) {
        if self.0.served.lock().unwrap().remove(feature).is_some() {
            self.0.features_changed();
        }
    }

    pub fn on_event(&self, listener: impl Fn(Event) + Send + Sync + 'static) {
        *self.0.events.lock().unwrap() = Some(Arc::new(listener));
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.0.sessions.lock().unwrap().values().cloned().collect()
    }

    pub fn session(&self, peer: &str) -> Option<Arc<Session>> {
        self.0.sessions.lock().unwrap().get(peer).cloned()
    }

    /// The sessions of devices no longer paired (forgotten by the user):
    /// over at once.
    pub fn check_paired(&self) {
        for session in self.sessions() {
            if !(self.0.paired)(&session.peer) {
                log!("{}: no longer paired", session.name);
                session.close();
            }
        }
    }

    /// Paired devices connect here: the address it took (port 0: any).
    pub fn listen(&self, address: SocketAddr) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind(address)?;
        let bound = listener.local_addr()?;
        self.0.listeners.lock().unwrap().push(listener.try_clone()?);
        let node = self.0.clone();
        std::thread::Builder::new().name("link-listen".into()).spawn(move || {
            for tcp in listener.incoming() {
                if node.closed.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(tcp) = tcp else {
                    // Out of descriptors, say: the connection waits, and so
                    // does this, not spinning meanwhile.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                // Anyone on the network can connect: the gate says who may
                // now (dropped unanswered).
                let Some(handshake) = tcp.peer_addr().ok().and_then(|from| node.gate.start(from.ip())) else { continue };
                let node = node.clone();
                // Each on its own: a slow or hostile one holds up no other.
                // (The handshake goes back when the thread ends, or if it
                // never starts.)
                let _ = std::thread::Builder::new().name("link-accept".into()).spawn(move || {
                    let from = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    crate::stream::tune_tcp(&tcp);
                    // No session within HANDSHAKE_WHOLE: given up by
                    // establish() (each of its reads and writes under it).
                    let result = node.accept(Box::new(tcp), "lan");
                    drop(handshake);
                    if let Err(e) = result {
                        log!("{from}: no session ({e})");
                    }
                });
            }
        })?;
        Ok(bound)
    }

    /// Our TCP paths to a device whose hello says it takes QUIC are followed
    /// by a QUIC path to it, which takes over (off at first: QUIC is still
    /// measured against TCP on each network).
    pub fn upgrade_to_quic(&self, on: bool) {
        self.0.quic_upgrade.store(on, Ordering::SeqCst);
    }

    /// Paired devices connect here by QUIC too (UDP: the same port as
    /// listen()'s TCP, say): the address it took.
    pub fn listen_quic(&self, address: SocketAddr) -> std::io::Result<SocketAddr> {
        let endpoint = crate::quic::endpoint(address, Some(&self.0.server))?;
        let bound = endpoint.local_addr()?;
        *self.0.quic.lock().unwrap() = Some(endpoint.clone());
        self.0.quic_port.store(bound.port(), Ordering::SeqCst);
        let node = self.0.clone();
        crate::quic::listen(endpoint, self.0.server.clone(), self.0.gate.clone(), session::HANDSHAKE_WHOLE,
                            move |stream, quic| {
            let from = quic.conn.remote_address();
            if node.closed.load(Ordering::SeqCst) {
                quic.close();
                return;
            }
            if let Err(e) = node.accept_quic(Box::new(stream), quic) {
                log!("{from}: no session by QUIC ({e})");
            }
        });
        Ok(bound)
    }

    /// A session (or a path for the one we have) with the device at the
    /// other end of stream, which connected to us (its TLS server: we).
    pub fn accept_stream(&self, stream: Box<dyn Stream>, kind: &str) -> Result<Arc<Session>, String> {
        self.0.accept(stream, kind).map_err(|e| e.to_string())
    }

    /// A session (or a path for the one we have) with the device at the
    /// other end of stream, which we reached (its TLS client: we).
    pub fn connect_stream(&self, stream: Box<dyn Stream>, kind: &str) -> Result<Arc<Session>, String> {
        let name = rustls::pki_types::ServerName::try_from("plus-one.invalid").unwrap();
        let conn = rustls::ClientConnection::new(self.0.client.clone(), name).map_err(|e| e.to_string())?;
        let established = session::establish(conn.into(), stream, true, self.0.hello(), kind, &|peer| self.0.ours(peer))
            .map_err(|e| e.to_string())?;
        self.0.add(established).map_err(|e| e.to_string())
    }

    /// Whether close() was called: nothing more is made on it.
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::SeqCst)
    }

    /// Everything over: no more listening, every session closed.
    pub fn close(&self) {
        // Under the sessions' lock: a session being added sees it, or is seen.
        let sessions = self.0.sessions.lock().unwrap();
        self.0.closed.store(true, Ordering::SeqCst);
        drop(sessions);
        for listener in self.0.listeners.lock().unwrap().drain(..) {
            // A listening socket shut down: its accept() returns at once.
            crate::sys::shutdown(listener.as_raw_fd());
        }
        if let Some(endpoint) = self.0.quic.lock().unwrap().take() {
            endpoint.close(0u32.into(), b"closed");
        }
        let sessions: Vec<Arc<Session>> = self.0.sessions.lock().unwrap().values().cloned().collect();
        for session in sessions {
            session.close();
        }
    }

    /// A session (or a path for the one we have) with the device listening
    /// at address.
    pub fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Arc<Session>, String> {
        self.connect_as(address, timeout, "lan")
    }

    /// A session (or a path for the one we have) with the paired device
    /// whose fingerprint is peer, through the relay at relay (the last path:
    /// both sides dial it). dialer: this side makes the session's TLS (the
    /// PC), the other takes it.
    pub fn relay(&self, relay: SocketAddr, peer: &str, dialer: bool, timeout: Duration) -> Result<Arc<Session>, String> {
        let ours = self.0.identity_fingerprint.clone();
        let stream = crate::relay::dial(relay, &crate::relay::rendezvous(&ours, peer), timeout)
            .map_err(|e| format!("the relay at {relay}: {e}"))?;
        if dialer {
            self.connect_stream(Box::new(stream), "relay")
        } else {
            self.accept_stream(Box::new(stream), "relay")
        }
    }

    /// As connect(), the path of that kind ("lan", "p2p": a Wi-Fi Direct
    /// group's, ranked above the LAN's).
    pub fn connect_as(&self, address: SocketAddr, timeout: Duration, kind: &str) -> Result<Arc<Session>, String> {
        let tcp = TcpStream::connect_timeout(&address, timeout).map_err(|e| format!("{address}: {e}"))?;
        crate::stream::tune_tcp(&tcp);
        self.connect_stream(Box::new(tcp), kind).map_err(|e| format!("{address}: {e}"))
    }

    /// As connect_as(), by QUIC alone.
    pub fn connect_quic(&self, address: SocketAddr, timeout: Duration, kind: &str) -> Result<Arc<Session>, String> {
        self.0.connect_quic(address, timeout, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::RawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Instant;

    fn identity() -> (Identity, String) {
        let made = rcgen::generate_simple_self_signed(vec!["plus-one".into()]).unwrap();
        let identity = Identity::new(made.cert.der().to_vec(), made.key_pair.serialize_der()).unwrap();
        let fingerprint = identity.fingerprint();
        (identity, fingerprint)
    }

    fn trusting(fingerprint: String) -> Paired {
        Arc::new(move |seen: &str| seen == fingerprint)
    }

    /// A stream that, as its session takes it for its path (its timeouts
    /// off: prepare()), says so and waits for a word.
    struct Held {
        stream: UnixStream,
        taken: mpsc::Sender<()>,
        go: Mutex<mpsc::Receiver<()>>,
    }

    impl Read for Held {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for Held {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.stream.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.stream.flush()
        }
    }

    impl AsRawFd for Held {
        fn as_raw_fd(&self) -> RawFd {
            self.stream.as_raw_fd()
        }
    }

    impl Stream for Held {
        fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
            self.stream.set_nonblocking(on)
        }

        fn set_timeouts(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            if timeout.is_none() {
                let _ = self.taken.send(());
                let _ = self.go.lock().unwrap().recv();
            }
            Stream::set_timeouts(&self.stream, timeout)
        }

        fn shutdown(&self) {
            Stream::shutdown(&self.stream)
        }

        fn try_clone(&self) -> std::io::Result<Box<dyn Stream>> {
            Ok(Box::new(self.stream.try_clone()?))
        }
    }

    #[test]
    fn a_session_over_before_it_is_kept_is_neither_kept_nor_told_up() {
        // A session ended (the other side's bye right behind its hello)
        // before add() kept it: kept all the same, closed, among the
        // node's for good (plus-one-link's devices showed it connected),
        // told Up and never Gone. Made sure of: the node's sessions held
        // from the moment the session has its path until it is over.
        let (pc_id, pc_fingerprint) = identity();
        let (tablet_id, tablet_fingerprint) = identity();
        let pc = Node::new("pc", &pc_id, trusting(tablet_fingerprint)).unwrap();
        let tablet = Node::new("tablet", &tablet_id, trusting(pc_fingerprint)).unwrap();
        let (tell, told) = mpsc::channel();
        let (path_of, running) = mpsc::channel();
        pc.on_event(move |event| match event {
            Event::Up(_) => drop(tell.send("up")),
            Event::Gone(..) => drop(tell.send("gone")),
            Event::Path(session, _) => drop(path_of.send(session)),
            _ => {}
        });
        let (ours, theirs) = UnixStream::pair().unwrap();
        let accepter = tablet.clone();
        std::thread::spawn(move || accepter.accept_stream(Box::new(theirs), "usb").map(|_| ()));
        let (taken, has_path) = mpsc::channel();
        let (go, going) = mpsc::channel();
        let connecting = {
            let pc = pc.clone();
            let held = Held { stream: ours, taken, go: Mutex::new(going) };
            std::thread::spawn(move || pc.connect_stream(Box::new(held), "usb"))
        };
        has_path.recv_timeout(Duration::from_secs(5)).expect("no handshake");
        let sessions = pc.0.sessions.lock().unwrap();
        go.send(()).unwrap();
        let session = running.recv_timeout(Duration::from_secs(5)).expect("no session's thread");
        let closing = session.clone();
        std::thread::spawn(move || closing.close());
        let started = Instant::now();
        while !session.is_closed() {
            assert!(started.elapsed() < Duration::from_secs(5), "never over");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(sessions);
        let made = connecting.join().unwrap();
        let mut events = Vec::new();
        while let Ok(event) = told.recv_timeout(Duration::from_millis(300)) {
            events.push(event);
        }
        assert_eq!(events, ["gone"], "told of a session over before it was kept");
        assert!(made.is_err(), "a session over before it was kept, given as made");
        assert!(pc.sessions().is_empty(), "a session over, kept");
        pc.close();
        tablet.close();
    }
}
