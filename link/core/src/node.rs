//! This side of the link: who we are, what we serve, our sessions (one per
//! paired device). It listens, connects, and tells who comes and goes.

use crate::identity::{Identity, Paired};
use crate::session::{self, Incoming, Session};
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

pub enum Event {
    /// A session with a paired device, up.
    Up(Arc<Session>),
    /// A device's session over: (its fingerprint, why).
    Gone(String, String),
}

pub(crate) struct NodeInner {
    name: String,
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    served: Mutex<HashMap<String, (u32, Handler)>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    events: Mutex<Option<Arc<dyn Fn(Event) + Send + Sync>>>,
    listeners: Mutex<Vec<TcpListener>>,
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

    /// A new session with a device: the one before it, if any, ends (it
    /// is the one that went quiet: the device came back by another way).
    fn add(self: &Arc<Self>, established: session::Established) -> std::io::Result<Arc<Session>> {
        let session = Session::start(self, established)?;
        let before = self.sessions.lock().unwrap().insert(session.peer.clone(), session.clone());
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
            server: identity.server_config(paired)?,
            served: Mutex::default(),
            sessions: Mutex::default(),
            events: Mutex::default(),
            listeners: Mutex::default(),
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
                let node = node.clone();
                // Each on its own: a slow or hostile one holds up no other.
                let _ = std::thread::Builder::new().name("link-accept".into()).spawn(move || {
                    let from = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    let result = rustls::ServerConnection::new(node.server.clone())
                        .map_err(|e| std::io::Error::other(e.to_string()))
                        .and_then(|conn| session::establish(conn.into(), tcp, false, node.hello()))
                        .and_then(|established| node.add(established));
                    if let Err(e) = result {
                        log!("{from}: no session ({e})");
                    }
                });
            }
        })?;
        Ok(bound)
    }

    /// Everything over: no more listening, every session closed.
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        for listener in self.0.listeners.lock().unwrap().drain(..) {
            // A listening socket shut down: its accept() returns at once.
            crate::sys::shutdown(listener.as_raw_fd());
        }
        let sessions: Vec<Arc<Session>> = self.0.sessions.lock().unwrap().values().cloned().collect();
        for session in sessions {
            session.close();
        }
    }

    /// A session with the device listening at address.
    pub fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Arc<Session>, String> {
        let tcp = TcpStream::connect_timeout(&address, timeout).map_err(|e| format!("{address}: {e}"))?;
        let name = rustls::pki_types::ServerName::try_from("plus-one.invalid").unwrap();
        let conn = rustls::ClientConnection::new(self.0.client.clone(), name).map_err(|e| e.to_string())?;
        let established = session::establish(conn.into(), tcp, true, self.0.hello()).map_err(|e| format!("{address}: {e}"))?;
        self.0.add(established).map_err(|e| e.to_string())
    }
}
