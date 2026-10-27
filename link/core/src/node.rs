//! This side of the link: who we are, what we serve, our sessions (one per
//! paired device). It listens, connects, and tells who comes and goes and
//! which way. A connection from a device we have a session with becomes
//! that session's path (both hellos name it), not a new session.

use crate::identity::{Identity, Paired};
use crate::session::{self, Incoming, Session};
use crate::stream::Stream;
use crate::log;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A feature's code for the channels the other side opens for it.
pub type Handler = Arc<dyn Fn(Incoming) + Send + Sync>;

/// Connections not yet a session, at once; more wait for none (closed).
const HANDSHAKES: usize = 8;
/// A connection that is not a session this long after it came: closed
/// (each read has its own timeout too; this is the whole of it).
const HANDSHAKE_WHOLE: Duration = Duration::from_secs(8);

pub enum Event {
    /// A session with a paired device, up.
    Up(Arc<Session>),
    /// Its path lost: it waits session::RESUME_WAIT for another (the
    /// dialer's to make).
    Detached(Arc<Session>),
    /// On another path now (session.path()).
    Moved(Arc<Session>),
    /// A device's session over: (its fingerprint, why).
    Gone(String, String),
}

pub(crate) struct NodeInner {
    name: String,
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    paired: Paired,
    served: Mutex<HashMap<String, (u32, Handler)>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    events: Mutex<Option<Arc<dyn Fn(Event) + Send + Sync>>>,
    listeners: Mutex<Vec<TcpListener>>,
    handshakes: AtomicUsize,
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
        json!({"t": "hello", "name": self.name, "features": self.features()})
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

    fn accept(self: &Arc<Self>, stream: Box<dyn Stream>, kind: &str) -> std::io::Result<Arc<Session>> {
        let conn = rustls::ServerConnection::new(self.server.clone()).map_err(|e| std::io::Error::other(e.to_string()))?;
        let established = session::establish(conn.into(), stream, false, self.hello(), kind, &|peer| self.ours(peer))?;
        self.add(established)
    }

    /// A connection made a session: a path for the one we have with the
    /// device (both hellos named it), or a new session, and the one
    /// before, if any, ends (the device forgot it: restarted, say).
    fn add(self: &Arc<Self>, established: session::Established) -> std::io::Result<Arc<Session>> {
        if let Some(id) = established.resumes.clone() {
            let existing = self.sessions.lock().unwrap().get(&established.peer).cloned();
            if let Some(existing) = existing.filter(|s| s.id == id) {
                let kind = established.kind().to_owned();
                if !existing.adopt(established) {
                    log!("{}: a {kind} path not taken (a better one is up)", existing.name);
                }
                return Ok(existing);
            }
        }
        let session = Session::start(self, established)?;
        let before = {
            let mut sessions = self.sessions.lock().unwrap();
            // Made while the node closed (close() saw none of it): not kept.
            if self.closed.load(Ordering::SeqCst) {
                drop(sessions);
                session.close();
                return Err(std::io::Error::other("the node is closed"));
            }
            sessions.insert(session.peer.clone(), session.clone())
        };
        // Served since the hello went (the session was not among them yet).
        session.our_features(self.features());
        if let Some(before) = before {
            before.close();
        }
        log!("{}: a session ({})", if session.name.is_empty() { &session.peer[..8] } else { &session.name },
             session.peer.get(..16).unwrap_or(""));
        self.event(Event::Up(session.clone()));
        Ok(session)
    }
}

#[derive(Clone)]
pub struct Node(Arc<NodeInner>);

impl Node {
    /// name: ours, as the other side shows it.
    pub fn new(name: &str, identity: &Identity, paired: Paired) -> Result<Node, String> {
        Ok(Node(Arc::new(NodeInner {
            name: name.to_owned(),
            client: identity.client_config(paired.clone())?,
            server: identity.server_config(paired.clone())?,
            paired,
            served: Mutex::default(),
            sessions: Mutex::default(),
            events: Mutex::default(),
            listeners: Mutex::default(),
            handshakes: AtomicUsize::new(0),
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
                let Ok(tcp) = tcp else { continue };
                // So many at once, no more: anyone on the network can connect.
                if node.handshakes.fetch_add(1, Ordering::SeqCst) >= HANDSHAKES {
                    node.handshakes.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let node = node.clone();
                // Each on its own: a slow or hostile one holds up no other.
                let _ = std::thread::Builder::new().name("link-accept".into()).spawn(move || {
                    let from = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    let _ = tcp.set_nodelay(true);
                    let done = Arc::new(AtomicBool::new(false));
                    if let Ok(watched) = tcp.try_clone() {
                        let done = done.clone();
                        let _ = std::thread::Builder::new().name("link-accept-watch".into()).spawn(move || {
                            std::thread::sleep(HANDSHAKE_WHOLE);
                            if !done.load(Ordering::SeqCst) {
                                let _ = watched.shutdown(std::net::Shutdown::Both);
                            }
                        });
                    }
                    let result = node.accept(Box::new(tcp), "lan");
                    done.store(true, Ordering::SeqCst);
                    node.handshakes.fetch_sub(1, Ordering::SeqCst);
                    if let Err(e) = result {
                        log!("{from}: no session ({e})");
                    }
                });
            }
        })?;
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
        let sessions: Vec<Arc<Session>> = self.0.sessions.lock().unwrap().values().cloned().collect();
        for session in sessions {
            session.close();
        }
    }

    /// A session (or a path for the one we have) with the device listening
    /// at address.
    pub fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Arc<Session>, String> {
        let tcp = TcpStream::connect_timeout(&address, timeout).map_err(|e| format!("{address}: {e}"))?;
        tcp.set_nodelay(true).map_err(|e| e.to_string())?;
        self.connect_stream(Box::new(tcp), "lan").map_err(|e| format!("{address}: {e}"))
    }
}
