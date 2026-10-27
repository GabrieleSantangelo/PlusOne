//! plus-one-link: Plus One's link as a program of its own (docs/LINK.md).
//! It keeps the sessions with the paired tablets; apps (Plus One's
//! features, in any language) ask it for channels through its socket.
//!
//!   plus-one-link [--socket PATH] [--config DIR] [--state FILE] [--name NAME] [--listen ADDRESS]
//!
//! The socket: $XDG_RUNTIME_DIR/plus-one/link (or systemd's). The identity
//! and the paired tablets: what Plus One keeps in ~/.config/plus-one/companion
//! (cert.pem, key.pem, paired.json), read as they are.
//!
//! Requests, each answered {"ok": true, ...} or {"ok": false, "error"}:
//!   {"op": "devices"}            the paired tablets: id, name, connected,
//!                                features, rtt_ms, dropped
//!   {"op": "connect", "address": "IP:PORT"|"device": ID[, "timeout": S]
//!    [, "kind": "p2p"]}          a session with the tablet there (or with
//!                                that one, wherever it is): its id; "p2p":
//!                                the address is in a Wi-Fi Direct group
//!                                with it (a better path, not remembered)
//!   {"op": "browse"[, "wait": S]}  the tablets announcing the link nearby
//!                                (mDNS): id (its start), name, address
//!   {"op": "queued", "device"}   bytes on their way to it, not yet sent
//!   {"op": "close", "device"}    its session over (its channels with it)
//!   {"op": "usb"[, "use": false]}  any Android tablet plugged in asked to be
//!                                Plus One's accessory (usb.rs), from now on;
//!                                one reached by cable before always is
//!   {"op": "open", "device", "feature", "channel", "class"}
//!                                a channel, its descriptor with the answer
//!                                (the session made first if there is none)
//!   {"op": "serve", "feature"[, "version"]}
//!                                the tablet's channels for this feature, from
//!                                then on, as {"event": "incoming", "device",
//!                                "feature", "channel", "class", "id"} with
//!                                each one's descriptor; until this
//!                                connection closes
//!   {"op": "watch"}              {"event": "up"|"gone", "device", ...} from
//!                                then on
//! A connection that served or watched carries only events after that.
//!
//! Sessions are made when an app needs one, where the tablet was last
//! reached or where mDNS finds it, and closed after IDLE with no channel:
//! nothing wakes the tablet's Wi-Fi while nothing is in use.

mod api;
mod usb;

use plusone_link::{log, Class, Event, Identity, Node};
use serde_json::{json, Value};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REQUEST: usize = 64 << 10;
const SERVICE: &str = "_plusone-link._tcp.local";
/// A session with no channel this long: closed.
const IDLE: Duration = Duration::from_secs(60);

struct Daemon {
    node: Node,
    config: PathBuf,
    /// Where each tablet was last reached: {id: "IP:PORT"}.
    state: PathBuf,
    finding: Mutex<()>,
    usb: usb::Watcher,
    /// The USB serial of the device last asked to be an accessory: its
    /// session, when it comes, remembers it.
    switched: Mutex<Option<String>>,
    watchers: Mutex<Vec<Arc<OwnedFd>>>,
    /// Who serves each feature now (a serial: the last one to ask).
    serving: Mutex<std::collections::HashMap<String, u64>>,
    serial: AtomicU64,
}

fn paired(config: &Path) -> serde_json::Map<String, Value> {
    std::fs::read(config.join("paired.json")).ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn paired_name(entry: &Value) -> &str {
    entry.as_str().or_else(|| entry["name"].as_str()).unwrap_or("")
}

impl Daemon {
    fn devices(&self) -> Value {
        let mut devices = Vec::new();
        let sessions = self.node.sessions();
        for (id, entry) in paired(&self.config) {
            let session = sessions.iter().find(|s| s.peer == id);
            devices.push(json!({
                "id": id,
                "name": session.map(|s| s.name.as_str()).filter(|n| !n.is_empty()).unwrap_or(paired_name(&entry)),
                "connected": session.is_some(),
                "features": session.map(|s| s.features()).unwrap_or(json!({})),
                "rtt_ms": session.and_then(|s| s.rtt()).map(|r| r.as_secs_f64() * 1000.0),
                "path": session.and_then(|s| s.path()),
                "dropped": session.map(|s| s.dropped()).unwrap_or(0),
            }));
        }
        json!({"ok": true, "devices": devices})
    }

    fn connect(&self, request: &Value) -> Result<Value, String> {
        let timeout = Duration::from_secs_f64(request["timeout"].as_f64().unwrap_or(5.0).clamp(0.1, 60.0));
        let session = match (request["address"].as_str(), request["device"].as_str()) {
            (Some(address), _) => {
                let address = address.parse().map_err(|e| format!("the address: {e}"))?;
                if request["kind"].as_str() == Some("p2p") {
                    self.node.connect_as(address, timeout, "p2p")?
                } else {
                    let session = self.node.connect(address, timeout)?;
                    self.reached(&session.peer, address);
                    session
                }
            }
            (None, Some(device)) => self.session(device)?,
            (None, None) => return Err("no address or device".into()),
        };
        Ok(json!({"ok": true, "device": session.peer, "name": session.name}))
    }

    fn remembered(&self) -> serde_json::Map<String, Value> {
        std::fs::read(&self.state).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    fn reached(&self, device: &str, address: std::net::SocketAddr) {
        let mut state = self.remembered();
        if state.get(device).and_then(Value::as_str) == Some(&address.to_string()) {
            return;
        }
        state.insert(device.to_owned(), json!(address.to_string()));
        self.save(state);
    }

    /// The session with a paired tablet: the one up, or a new one where it
    /// was last reached and wherever mDNS finds it, at once (the first to
    /// answer wins).
    fn session(&self, device: &str) -> Result<Arc<plusone_link::Session>, String> {
        // One up, or waiting for its next path (what is opened on it waits too).
        if let Some(session) = self.node.session(device).filter(|s| !s.is_closed()) {
            return Ok(session);
        }
        self.reach(device)
    }

    /// The tablet, reached anew: where it was last reached, else wherever
    /// mDNS finds it (looked for meanwhile, connected to only if the first
    /// failed: never two at once). A session it has with us goes on there.
    fn reach(&self, device: &str) -> Result<Arc<plusone_link::Session>, String> {
        if !paired(&self.config).contains_key(device) {
            return Err("not a paired tablet".into());
        }
        let _one_at_a_time = self.finding.lock().unwrap();
        if let Some(session) = self.node.session(device).filter(|s| !s.is_closed() && s.path().is_some()) {
            return Ok(session);  // reached while this waited
        }
        let id = device.to_owned();
        let looking = std::thread::spawn(move || {
            let mine = |f: &plusone_link::mdns::Found| f.txt.get("id").is_some_and(|i| i.len() >= 8 && id.starts_with(i.as_str()));
            plusone_link::mdns::browse(SERVICE, Duration::from_secs(3), mine).map_err(|e| e.to_string())
                .and_then(|all| all.into_iter().find(|f| mine(f)).map(|f| f.address).ok_or_else(|| "not found nearby".to_owned()))
        });
        let mut why = Vec::new();
        let remembered: Option<std::net::SocketAddr> =
            self.remembered().get(device).and_then(Value::as_str).and_then(|a| a.parse().ok());
        let attempt = |address: std::net::SocketAddr, why: &mut Vec<String>| match self.node.connect(address, Duration::from_millis(1500)) {
            Ok(session) if session.peer == device => {
                self.reached(device, address);
                Some(session)
            }
            Ok(session) => {
                why.push(format!("{} answered at {address}", &session.peer[..8]));
                None
            }
            Err(e) => {
                why.push(e);
                None
            }
        };
        if let Some(session) = remembered.and_then(|address| attempt(address, &mut why)) {
            return Ok(session);
        }
        match looking.join() {
            Ok(Ok(address)) if Some(address) != remembered => {
                if let Some(session) = attempt(address, &mut why) {
                    return Ok(session);
                }
            }
            Ok(Ok(_)) => {}  // where the first try was
            Ok(Err(e)) => why.push(e),
            Err(_) => why.push("mDNS failed".into()),
        }
        Err(if why.is_empty() { "not reached".into() } else { why.join("; ") })
    }

    fn browse(&self, request: &Value) -> Result<Value, String> {
        let wait = Duration::from_secs_f64(request["wait"].as_f64().unwrap_or(1.5).clamp(0.1, 10.0));
        let found = plusone_link::mdns::browse(SERVICE, wait, |_| false).map_err(|e| e.to_string())?;
        let tablets: Vec<Value> = found.iter().map(|f| json!({
            "id": f.txt.get("id"), "name": f.txt.get("name"), "version": f.txt.get("v"),
            "address": f.address.to_string()})).collect();
        Ok(json!({"ok": true, "tablets": tablets}))
    }

    /// The cable, every usb::EVERY.
    fn watch_usb(self: &Arc<Self>) {
        let me = self.clone();
        let connect: Arc<dyn Fn(std::os::unix::net::UnixStream, &usb::Device) -> Result<String, String> + Send + Sync> =
            Arc::new(move |stream, _device| {
                let session = me.node.connect_stream(Box::new(stream), "usb")?;
                log!("usb: a session with {} by cable", session.name);
                if let Some(serial) = me.switched.lock().unwrap().take().filter(|s| !s.is_empty()) {
                    me.remember_usb(&serial);
                }
                // While it runs on the cable (it may take a moment to move
                // over): then the cable may carry the next.
                let since = std::time::Instant::now();
                let mut on_it = false;
                while !session.is_closed() {
                    if session.path().as_deref() == Some("usb") {
                        on_it = true;
                    } else if on_it || since.elapsed() > Duration::from_secs(5) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
                Ok(session.peer.clone())
            });
        loop {
            std::thread::sleep(usb::EVERY);
            let state = self.remembered();
            let serials: Vec<String> = state.get("usb").and_then(Value::as_array).into_iter().flatten()
                .filter_map(|s| s.as_str().map(str::to_owned)).collect();
            self.usb.look(&|serial| serials.iter().any(|s| s == serial),
                          &|device| *self.switched.lock().unwrap() = Some(device.serial.clone()),
                          connect.clone());
        }
    }

    fn remember_usb(&self, serial: &str) {
        let mut state = self.remembered();
        let mut serials: Vec<Value> = state.get("usb").and_then(Value::as_array).cloned().unwrap_or_default();
        if serials.iter().any(|s| s.as_str() == Some(serial)) {
            return;
        }
        serials.push(json!(serial));
        state.insert("usb".into(), Value::Array(serials));
        self.save(state);
    }

    fn save(&self, state: serde_json::Map<String, Value>) {
        if let Some(parent) = self.state.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let part = self.state.with_extension("part");
        if std::fs::write(&part, Value::Object(state).to_string()).is_ok() {
            let _ = std::fs::rename(&part, &self.state);
        }
    }

    fn queued(&self, request: &Value) -> Result<Value, String> {
        let session = self.node.session(request["device"].as_str().ok_or("no device")?).ok_or("not connected")?;
        Ok(json!({"ok": true, "queued": session.queued()}))
    }

    /// Sessions with no channel for IDLE: closed.
    fn reap(&self) {
        let mut idle: std::collections::HashMap<String, std::time::Instant> = std::collections::HashMap::new();
        loop {
            std::thread::sleep(IDLE / 4);
            let now = std::time::Instant::now();
            // Forgotten by the user meanwhile: over now.
            self.node.check_paired();
            let sessions = self.node.sessions();
            idle.retain(|peer, _| sessions.iter().any(|s| &s.peer == peer));
            for session in sessions {
                // By cable: kept (the tablet charges; its Wi-Fi is not woken).
                if session.channels() > 0 || session.path().as_deref() == Some("usb") {
                    idle.remove(&session.peer);
                } else if now.duration_since(*idle.entry(session.peer.clone()).or_insert(now)) >= IDLE {
                    log!("{}: idle, closed", session.name);
                    session.close();
                }
            }
        }
    }

    fn open(&self, request: &Value) -> Result<(Value, OwnedFd), String> {
        let device = request["device"].as_str().ok_or("no device")?;
        let session = self.session(device)?;
        let class = request["class"].as_str().and_then(Class::parse).ok_or("no class")?;
        let feature = request["feature"].as_str().ok_or("no feature")?;
        let channel = session.open(feature, request["channel"].as_str().unwrap_or(""), class)?;
        Ok((json!({"ok": true, "id": channel.id}), channel.into_fd()))
    }

    fn serve(self: &Arc<Self>, request: &Value, client: Arc<OwnedFd>) -> Result<String, String> {
        let feature = request["feature"].as_str().filter(|f| !f.is_empty()).ok_or("no feature")?.to_owned();
        let version = request["version"].as_u64().unwrap_or(1) as u32;
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        self.serving.lock().unwrap().insert(feature.clone(), serial);
        self.node.serve(&feature, version, move |incoming| {
            let channel = incoming.channel;
            let event = json!({"event": "incoming", "device": incoming.session.peer, "feature": channel.feature,
                               "channel": channel.name, "class": channel.class.name(), "id": channel.id});
            // The app gone meanwhile: the channel closes with ours.
            let _ = api::send(client.as_raw_fd(), &event, Some(channel.into_fd()));
        });
        Ok(feature)
    }

    fn event(self: &Arc<Self>, event: Event) {
        let message = match event {
            Event::Up(session) => json!({"event": "up", "device": session.peer, "name": session.name,
                                         "features": session.features(), "path": session.path()}),
            Event::Moved(session) => json!({"event": "moved", "device": session.peer, "path": session.path()}),
            Event::Detached(session) => {
                self.come_back(session.clone());
                json!({"event": "detached", "device": session.peer})
            }
            Event::Gone(device, reason) => json!({"event": "gone", "device": device, "reason": reason}),
        };
        // Never waiting on an app: one that does not read loses its events.
        self.watchers.lock().unwrap().retain(|watcher| api::send_now(watcher.as_raw_fd(), &message).is_ok());
    }

    /// A session's path lost: another made at once, the network's (the
    /// cable's, if it is plugged in, comes by itself: usb.rs), until it
    /// is on again or over.
    fn come_back(self: &Arc<Self>, session: Arc<plusone_link::Session>) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("link-come-back".into()).spawn(move || {
            while !session.is_closed() && session.path().is_none() {
                match me.reach(&session.peer) {
                    Ok(_) => return,
                    Err(e) => log!("{}: not reached yet ({e})", session.name),
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    /// One app's connection: its requests, until it goes.
    fn client(self: Arc<Self>, client: OwnedFd) {
        let client = Arc::new(client);
        let mut buf = vec![0u8; REQUEST];
        let mut served: Vec<(String, u64)> = Vec::new();
        while let Ok(Some(length)) = api::receive(client.as_raw_fd(), &mut buf) {
            let request: Value = match serde_json::from_slice(&buf[..length]) {
                Ok(request) => request,
                Err(e) => {
                    let _ = api::send(client.as_raw_fd(), &json!({"ok": false, "error": e.to_string()}), None);
                    continue;
                }
            };
            let (reply, fd) = match request["op"].as_str().unwrap_or("") {
                "devices" => (Ok(self.devices()), None),
                "connect" => (self.connect(&request), None),
                "browse" => (self.browse(&request), None),
                "queued" => (self.queued(&request), None),
                "usb" => {
                    self.usb.asked.store(request["use"].as_bool().unwrap_or(true), Ordering::Relaxed);
                    (Ok(json!({"ok": true})), None)
                }
                "close" => {
                    if let Some(session) = request["device"].as_str().and_then(|d| self.node.session(d)) {
                        session.close();
                    }
                    (Ok(json!({"ok": true})), None)
                }
                "open" => match self.open(&request) {
                    Ok((reply, fd)) => (Ok(reply), Some(fd)),
                    Err(e) => (Err(e), None),
                },
                "serve" => match self.serve(&request, client.clone()) {
                    Ok(feature) => {
                        let serial = self.serving.lock().unwrap()[&feature];
                        served.push((feature, serial));
                        (Ok(json!({"ok": true})), None)
                    }
                    Err(e) => (Err(e), None),
                },
                "watch" => {
                    self.watchers.lock().unwrap().push(client.clone());
                    (Ok(json!({"ok": true})), None)
                }
                other => (Err(format!("no such request: {other:?}")), None),
            };
            let reply = reply.unwrap_or_else(|e| json!({"ok": false, "error": e}));
            if api::send(client.as_raw_fd(), &reply, fd).is_err() {
                break;
            }
        }
        // Gone: what it served (unless another app took it over since), its events.
        for (feature, serial) in served {
            let mut serving = self.serving.lock().unwrap();
            if serving.get(&feature) == Some(&serial) {
                serving.remove(&feature);
                self.node.unserve(&feature);
            }
        }
        self.watchers.lock().unwrap().retain(|watcher| !Arc::ptr_eq(watcher, &client));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let home = std::env::var("HOME").unwrap_or_default();
    let config = PathBuf::from(value("--config").unwrap_or_else(|| {
        let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
        format!("{base}/plus-one/companion")
    }));
    let socket = PathBuf::from(value("--socket").unwrap_or_else(|| {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        format!("{runtime}/plus-one/link")
    }));
    let name = value("--name").unwrap_or_else(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_owned()).unwrap_or_default()
    });
    let identity = match Identity::from_pem_files(&config.join("cert.pem").to_string_lossy(),
                                                  &config.join("key.pem").to_string_lossy()) {
        Ok(identity) => identity,
        Err(e) => {
            log!("no identity ({e}): Plus One makes it on first use");
            std::process::exit(1);
        }
    };
    let paired_in = config.clone();
    let node = match Node::new(&name, &identity, Arc::new(move |fp: &str| paired(&paired_in).contains_key(fp))) {
        Ok(node) => node,
        Err(e) => {
            log!("{e}");
            std::process::exit(1);
        }
    };
    let state = PathBuf::from(value("--state").unwrap_or_else(|| {
        let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| format!("{home}/.local/state"));
        format!("{base}/plus-one/link.json")
    }));
    let daemon = Arc::new(Daemon { node: node.clone(), config, state, finding: Mutex::default(), usb: usb::Watcher::new(),
                                   switched: Mutex::default(), watchers: Mutex::default(), serving: Mutex::default(),
                                   serial: AtomicU64::new(0) });
    let cable = daemon.clone();
    std::thread::Builder::new().name("link-usb".into()).spawn(move || cable.watch_usb()).unwrap();
    let reaper = daemon.clone();
    std::thread::Builder::new().name("link-idle".into()).spawn(move || reaper.reap()).unwrap();
    let events = daemon.clone();
    node.on_event(move |event| events.event(event));
    if let Some(address) = value("--listen") {
        match address.parse().map_err(|e| format!("{e}")).and_then(|a| node.listen(a).map_err(|e| e.to_string())) {
            Ok(bound) => log!("listening at {bound}"),
            Err(e) => log!("cannot listen at {address}: {e}"),
        }
    }
    let listener = match api::listen(&socket) {
        Ok(listener) => listener,
        Err(e) => {
            log!("{}: {e}", socket.display());
            std::process::exit(1);
        }
    };
    log!("apps at {}", socket.display());
    loop {
        match api::accept(&listener) {
            Ok(Some(client)) => {
                let daemon = daemon.clone();
                let _ = std::thread::Builder::new().name("link-app".into()).spawn(move || daemon.client(client));
            }
            Ok(None) => log!("an app of another user, shut out"),
            Err(e) => {
                log!("accept: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
