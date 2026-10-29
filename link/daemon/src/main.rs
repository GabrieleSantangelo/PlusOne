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
//!                                features, rtt_ms, path, quic (its path a
//!                                QUIC one), transport ("quic", "tcp",
//!                                "stream": a cable's, Bluetooth's; null
//!                                with no path), rate_kbps (its
//!                                controller's), dropped, paths (each:
//!                                id, kind, transport, state "active",
//!                                "suspect" or "failed", rtt_ms,
//!                                silent_ms, budget_bps, budget_from)
//!   {"op": "connect", "address": "IP:PORT"|"device": ID[, "timeout": S]
//!    [, "kind": "p2p"|"ap"]}     a session with the tablet there (or with
//!                                that one, wherever it is): its id; "p2p",
//!                                "ap": the address is in a Wi-Fi Direct
//!                                group with it, on the PC's own access
//!                                point (a better path, not remembered)
//!   {"op": "stream", "kind": "bt"} with a stream socket's descriptor: a
//!                                path on it (a Bluetooth socket BlueZ gave
//!                                the app), the session made on it
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
//!   {"op": "watch"}              {"event": "up"|"moved"|"detached"|"gone",
//!                                "device", ...} from then on (up and moved
//!                                with "path" and "transport", as devices';
//!                                a TCP path moved to QUIC: "moved"); and
//!                                {"event": "budget", "device", "kbps"}: what
//!                                its path's congestion controller allows,
//!                                as it moves (past 10 %, a cut at once:
//!                                architecture §3.11), null on a path with
//!                                none of its own (TCP, a cable); and
//!                                {"event": "path", "device", ...} as a
//!                                path's state changes (as devices' paths)
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
    /// This PC's fingerprint: the tags the apps announce are for it (mdns::tag).
    ours: String,
    /// Where each tablet was last reached: {id: "IP:PORT"}.
    state: PathBuf,
    /// Held while state is read, changed and written again.
    changing: Mutex<()>,
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
    fn new(node: Node, config: PathBuf, ours: String, state: PathBuf) -> Daemon {
        Daemon { node, config, ours, state, changing: Mutex::default(), finding: Mutex::default(), usb: usb::Watcher::new(),
                 switched: Mutex::default(), watchers: Mutex::default(), serving: Mutex::default(), serial: AtomicU64::new(0) }
    }

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
                "quic": session.is_some_and(|s| s.by_quic()),
                "transport": session.and_then(|s| s.transport()),
                "rate_kbps": session.and_then(|s| s.rate()).map(|r| r * 8 / 1000),
                "dropped": session.map(|s| s.dropped()).unwrap_or(0),
                "paths": session.map(|s| s.paths()).unwrap_or_default().iter().map(|p| p.json()).collect::<Vec<_>>(),
            }));
        }
        json!({"ok": true, "devices": devices})
    }

    fn connect(&self, request: &Value) -> Result<Value, String> {
        let timeout = request["timeout"].as_f64().filter(|t| t.is_finite()).unwrap_or(5.0).clamp(0.1, 60.0);
        let timeout = Duration::from_secs_f64(timeout);
        let session = match (request["address"].as_str(), request["device"].as_str()) {
            (Some(address), _) => {
                let address = address.parse().map_err(|e| format!("the address: {e}"))?;
                if let Some(kind @ ("p2p" | "ap")) = request["kind"].as_str() {
                    self.node.connect_as(address, timeout, kind)?
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

    /// A path on a stream an app made and hands over (its descriptor with
    /// the request): a Bluetooth socket ("bt"), the PC making the session.
    fn stream(&self, request: &Value, given: Option<OwnedFd>) -> Result<Value, String> {
        let fd = given.ok_or("no descriptor")?;
        let kind = match request["kind"].as_str() {
            Some(kind @ "bt") => kind,
            _ => return Err("not a kind of stream path".into()),
        };
        let stream = std::os::unix::net::UnixStream::from(fd);  // read, write, shut down: any stream socket
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;  // as it came: the handshake waits on it
        let session = self.node.connect_stream(Box::new(stream), kind)?;
        Ok(json!({"ok": true, "device": session.peer, "name": session.name}))
    }

    fn remembered(&self) -> serde_json::Map<String, Value> {
        std::fs::read(&self.state).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    fn reached(&self, device: &str, address: std::net::SocketAddr) {
        let address = address.to_string();
        self.change(|state| {
            if state.get(device).and_then(Value::as_str) == Some(address.as_str()) {
                return false;
            }
            state.insert(device.to_owned(), json!(address));
            true
        });
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
        let (id, ours) = (device.to_owned(), self.ours.clone());
        let looking = std::thread::spawn(move || {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let mine = |f: &plusone_link::mdns::Found| plusone_link::mdns::announces(f, &id, &ours, now);
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
        let wait = request["wait"].as_f64().filter(|w| w.is_finite()).unwrap_or(1.5).clamp(0.1, 10.0);
        let wait = Duration::from_secs_f64(wait);
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
        self.change(|state| {
            let mut serials: Vec<Value> = state.get("usb").and_then(Value::as_array).cloned().unwrap_or_default();
            if serials.iter().any(|s| s.as_str() == Some(serial)) {
                return false;
            }
            serials.push(json!(serial));
            state.insert("usb".into(), Value::Array(serials));
            true
        });
    }

    /// The state as change leaves it (it says whether it changed it), one
    /// change at a time.
    fn change(&self, change: impl FnOnce(&mut serde_json::Map<String, Value>) -> bool) {
        let _one_at_a_time = self.changing.lock().unwrap();
        let mut state = self.remembered();
        if !change(&mut state) {
            return;
        }
        if let Some(parent) = self.state.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A file of each save's own (another plus-one-link may save too),
        // in its place whole.
        static SAVES: AtomicU64 = AtomicU64::new(0);
        let part = self.state.with_extension(format!("{}.{}.part", std::process::id(), SAVES.fetch_add(1, Ordering::Relaxed)));
        if std::fs::write(&part, Value::Object(state).to_string()).is_err() || std::fs::rename(&part, &self.state).is_err() {
            let _ = std::fs::remove_file(&part);
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

    /// The feature, and the serial it is served under for this app.
    fn serve(self: &Arc<Self>, request: &Value, client: Arc<OwnedFd>) -> Result<(String, u64), String> {
        let feature = request["feature"].as_str().filter(|f| !f.is_empty()).ok_or("no feature")?.to_owned();
        let version = request.get("version").map_or(Some(1), |v| v.as_u64().and_then(|v| u32::try_from(v).ok()))
            .ok_or("the version: a whole number under 2^32")?;
        // The serial and the node's handler of the same app, whoever comes last.
        let mut serving = self.serving.lock().unwrap();
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        serving.insert(feature.clone(), serial);
        self.node.serve(&feature, version, move |incoming| {
            let channel = incoming.channel;
            let event = json!({"event": "incoming", "device": incoming.session.peer, "feature": channel.feature,
                               "channel": channel.name, "class": channel.class.name(), "id": channel.id});
            // The app gone meanwhile: the channel closes with ours.
            let _ = api::send(client.as_raw_fd(), &event, Some(channel.into_fd()));
        });
        Ok((feature, serial))
    }

    fn event(self: &Arc<Self>, event: Event) {
        // On a path with no controller of its own: no budget (the apps
        // follow their own way); on QUIC its controller tells it soon.
        let no_budget = |session: &plusone_link::Session| {
            (!session.by_quic()).then(|| json!({"event": "budget", "device": session.peer, "kbps": null}))
        };
        let (message, also) = match event {
            Event::Up(session) => (json!({"event": "up", "device": session.peer, "name": session.name,
                                          "features": session.features(), "path": session.path(),
                                          "transport": session.transport()}),
                                   no_budget(&session)),
            Event::Moved(session) => (json!({"event": "moved", "device": session.peer, "path": session.path(),
                                             "transport": session.transport()}),
                                      no_budget(&session)),
            Event::Budget(session, rate) => {
                (json!({"event": "budget", "device": session.peer, "kbps": rate * 8 / 1000}), None)
            }
            Event::Path(session, path) => {
                let mut message = path.json();
                message["event"] = json!("path");
                message["device"] = json!(session.peer);
                (message, None)
            }
            Event::Detached(session) => {
                self.come_back(session.clone());
                (json!({"event": "detached", "device": session.peer}), None)
            }
            Event::Gone(device, reason) => (json!({"event": "gone", "device": device, "reason": reason}), None),
        };
        for message in std::iter::once(message).chain(also) {
            self.tell(&message);
        }
    }

    /// An event to the apps that watch, never waiting on one: one that does
    /// not read loses it; one that cannot be told (gone, broken) watches no
    /// more, and its connection is shut: the app sees its end.
    fn tell(&self, message: &Value) {
        self.watchers.lock().unwrap().retain(|watcher| match api::send_now(watcher.as_raw_fd(), message) {
            Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                api::hang_up(watcher.as_raw_fd());
                false
            }
            _ => true,
        });
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
        while let Ok(Some(received)) = api::receive(client.as_raw_fd(), &mut buf) {
            let request = match received {
                Ok((length, given)) => serde_json::from_slice::<Value>(&buf[..length]).map(|r| (r, given)).map_err(|e| e.to_string()),
                Err(refused) => Err(refused.to_owned()),
            };
            let (request, given) = match request {
                Ok(request) => request,
                Err(e) => {
                    let _ = api::send(client.as_raw_fd(), &json!({"ok": false, "error": e}), None);
                    continue;
                }
            };
            let (reply, fd) = match request["op"].as_str().unwrap_or("") {
                "devices" => (Ok(self.devices()), None),
                "connect" => (self.connect(&request), None),
                "stream" => (self.stream(&request, given), None),
                "browse" => (self.browse(&request), None),
                "queued" => (self.queued(&request), None),
                "usb" => match request.get("use").map_or(Some(true), Value::as_bool) {
                    Some(on) => {
                        self.usb.asked.store(on, Ordering::Relaxed);
                        (Ok(json!({"ok": true})), None)
                    }
                    None => (Err("use: true or false".into()), None),
                },
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
                    Ok(serving) => {
                        served.push(serving);
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
    let daemon = Arc::new(Daemon::new(node.clone(), config, identity.fingerprint(), state));
    let cable = daemon.clone();
    std::thread::Builder::new().name("link-usb".into()).spawn(move || cable.watch_usb()).unwrap();
    let reaper = daemon.clone();
    std::thread::Builder::new().name("link-idle".into()).spawn(move || reaper.reap()).unwrap();
    let events = daemon.clone();
    node.on_event(move |event| events.event(event));
    // PLUSONE_LINK_QUIC=1: a TCP session moves to QUIC where the tablet
    // takes it (its hello says so); off until it beats TCP everywhere.
    if std::env::var("PLUSONE_LINK_QUIC").as_deref() == Ok("1") {
        node.upgrade_to_quic(true);
        log!("QUIC: TCP paths move to it where the tablet takes it");
    }
    if let Some(address) = value("--listen") {
        match address.parse().map_err(|e| format!("{e}")).and_then(|a| node.listen(a).map_err(|e| e.to_string())) {
            Ok(bound) => {
                log!("listening at {bound}");
                match node.listen_quic(bound) {
                    Ok(quic) => log!("QUIC at {quic}"),
                    Err(e) => log!("no QUIC at {bound}: {e}"),
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    fn identity() -> Identity {
        let made = rcgen::generate_simple_self_signed(vec!["plus-one".into()]).unwrap();
        Identity::new(made.cert.der().to_vec(), made.key_pair.serialize_der()).unwrap()
    }

    /// A folder of the test's own, empty.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("plus-one-link-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    /// A daemon in a folder of its own, with nothing paired.
    fn daemon(name: &str) -> Arc<Daemon> {
        let folder = folder(name);
        let identity = identity();
        let node = Node::new("pc", &identity, Arc::new(|_: &str| false)).unwrap();
        Arc::new(Daemon::new(node, folder.clone(), identity.fingerprint(), folder.join("link.json")))
    }

    /// A daemon in a folder of its own with a tablet paired, and the
    /// tablet.
    fn paired_daemon(name: &str) -> (Arc<Daemon>, Node) {
        let folder = folder(name);
        let (pc, tablet) = (identity(), identity());
        let (pc_fingerprint, tablet_fingerprint) = (pc.fingerprint(), tablet.fingerprint());
        std::fs::write(folder.join("paired.json"), json!({tablet_fingerprint: "tablet"}).to_string()).unwrap();
        let config = folder.clone();
        let node = Node::new("pc", &pc, Arc::new(move |fp: &str| paired(&config).contains_key(fp))).unwrap();
        let daemon = Arc::new(Daemon::new(node, folder.clone(), pc_fingerprint.clone(), folder.join("link.json")));
        let events = daemon.clone();
        daemon.node.on_event(move |event| events.event(event));
        let tablet = Node::new("tablet", &tablet, Arc::new(move |fp: &str| fp == pc_fingerprint)).unwrap();
        (daemon, tablet)
    }

    /// The tablet's session with the daemon over a stream an app hands it,
    /// as Bluetooth's: its id.
    fn by_bluetooth(daemon: &Arc<Daemon>, tablet: &Node) -> String {
        let (app, _served) = app(daemon);
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let accepter = tablet.clone();
        std::thread::spawn(move || accepter.accept_stream(Box::new(theirs), "bt").map(|_| ()));
        api::send_fds(app.as_raw_fd(), br#"{"op": "stream", "kind": "bt"}"#, &[ours.as_raw_fd()]);
        drop(ours);
        let reply = heard(&app).expect("no reply");
        assert_eq!(reply["ok"], true, "{reply}");
        reply["device"].as_str().unwrap().to_owned()
    }

    /// The events an app watching hears, until one of each kind in kinds
    /// came (the last of each).
    fn events_until(watching: &OwnedFd, kinds: &[&str]) -> std::collections::HashMap<String, Value> {
        let mut seen = std::collections::HashMap::new();
        while kinds.iter().any(|kind| !seen.contains_key(*kind)) {
            let event = heard(watching).unwrap_or_else(|| panic!("not all of {kinds:?} told: {seen:?}"));
            seen.insert(event["event"].as_str().unwrap_or("").to_owned(), event);
        }
        seen
    }

    /// An app's connection to it, served on a thread of its own.
    fn app(daemon: &Arc<Daemon>) -> (OwnedFd, std::thread::JoinHandle<()>) {
        let mut pair = [0; 2];
        assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, pair.as_mut_ptr()) }, 0);
        let (app, ours) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        let wait = libc::timeval { tv_sec: 5, tv_usec: 0 };
        unsafe {
            libc::setsockopt(pair[0], libc::SOL_SOCKET, libc::SO_RCVTIMEO, &wait as *const libc::timeval as *const libc::c_void,
                             std::mem::size_of::<libc::timeval>() as libc::socklen_t);
        }
        let daemon = daemon.clone();
        (app, std::thread::spawn(move || daemon.client(ours)))
    }

    /// The next message the app reads (None: nothing within 5 s, or its end).
    fn heard(app: &OwnedFd) -> Option<Value> {
        let mut buf = vec![0u8; REQUEST];
        let (length, _) = api::receive(app.as_raw_fd(), &mut buf).ok()??.ok()?;
        serde_json::from_slice(&buf[..length]).ok()
    }

    fn ask(app: &OwnedFd, request: Value) -> Value {
        api::send(app.as_raw_fd(), &request, None).unwrap();
        heard(app).expect("no reply")
    }

    /// Whether the app's connection ended (within 5 s).
    fn ended(app: &OwnedFd) -> bool {
        let mut byte = 0u8;
        unsafe { libc::recv(app.as_raw_fd(), &mut byte as *mut u8 as *mut libc::c_void, 1, 0) == 0 }
    }

    #[test]
    fn a_watcher_that_falls_behind_loses_events_not_its_watch() {
        let daemon = daemon("behind");
        let (app, _served) = app(&daemon);
        assert_eq!(ask(&app, json!({"op": "watch"}))["ok"], true);
        let budget = json!({"event": "budget", "device": "a".repeat(64), "kbps": 12345});
        for _ in 0..10_000 {  // far more than its connection holds
            daemon.tell(&budget);
        }
        assert_eq!(daemon.watchers.lock().unwrap().len(), 1);
        let mut buf = vec![0u8; REQUEST];
        let mut unread = 0;
        while unsafe { libc::recv(app.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT) } > 0 {
            unread += 1;
        }
        assert!(unread > 0 && unread < 10_000, "{unread}");
        let gone = json!({"event": "gone", "device": "a".repeat(64), "reason": "bye"});
        daemon.tell(&gone);
        assert_eq!(heard(&app), Some(gone));
    }

    #[test]
    fn a_watcher_that_cannot_be_told_sees_its_end() {
        let daemon = daemon("broken");
        let (app, served) = app(&daemon);
        assert_eq!(ask(&app, json!({"op": "watch"}))["ok"], true);
        daemon.tell(&json!({"event": "budget", "device": "a".repeat(1 << 20)}));  // more than a connection ever holds
        assert!(daemon.watchers.lock().unwrap().is_empty());
        assert!(ended(&app));
        served.join().unwrap();
    }

    #[test]
    fn a_request_with_too_many_descriptors_refused() {
        let daemon = daemon("descriptors");
        let (app, _served) = app(&daemon);
        let null = std::fs::File::open("/dev/null").unwrap();
        api::send_fds(app.as_raw_fd(), br#"{"op": "devices"}"#, &[null.as_raw_fd(); 8]);
        let reply = heard(&app).expect("no reply");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "too many descriptors");
        assert_eq!(ask(&app, json!({"op": "devices"}))["ok"], true);
    }

    #[test]
    fn each_app_serving_a_feature_leaves_it_once() {
        let daemon = daemon("serving");
        let start = std::sync::Barrier::new(2);
        for _ in 0..300 {
            let apps = [app(&daemon), app(&daemon)];
            std::thread::scope(|scope| {
                for (app, _) in &apps {
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        assert_eq!(ask(app, json!({"op": "serve", "feature": "x"}))["ok"], true);
                    });
                }
            });
            // Each app's leaving, the feature served as the last one asked
            // before it: the last one takes it away, the other not.
            let last = daemon.serving.lock().unwrap()["x"];
            let mut taken = 0;
            for (app, served) in apps {
                daemon.serving.lock().unwrap().insert("x".into(), last);
                drop(app);
                served.join().unwrap();
                taken += usize::from(!daemon.serving.lock().unwrap().contains_key("x"));
            }
            assert_eq!(taken, 1);
            daemon.serving.lock().unwrap().clear();
        }
    }

    #[test]
    fn a_version_that_is_no_u32_refused() {
        let daemon = daemon("version");
        let (app, _served) = app(&daemon);
        for version in [json!(1u64 << 32), json!(-1), json!("2"), json!(1.5)] {
            let reply = ask(&app, json!({"op": "serve", "feature": "x", "version": version}));
            assert_eq!(reply["ok"], false, "{version}");
        }
        assert!(daemon.serving.lock().unwrap().is_empty());
        assert_eq!(ask(&app, json!({"op": "serve", "feature": "x", "version": 3}))["ok"], true);
    }

    #[test]
    fn usb_use_only_true_or_false() {
        let daemon = daemon("usb");
        let (app, _served) = app(&daemon);
        for given in [json!("false"), json!(0), json!(null)] {
            assert_eq!(ask(&app, json!({"op": "usb", "use": given}))["ok"], false, "{given}");
            assert!(!daemon.usb.asked.load(Ordering::Relaxed), "{given}");
        }
        assert_eq!(ask(&app, json!({"op": "usb", "use": true}))["ok"], true);
        assert!(daemon.usb.asked.load(Ordering::Relaxed));
        assert_eq!(ask(&app, json!({"op": "usb", "use": false}))["ok"], true);
        assert!(!daemon.usb.asked.load(Ordering::Relaxed));
        assert_eq!(ask(&app, json!({"op": "usb"}))["ok"], true);
        assert!(daemon.usb.asked.load(Ordering::Relaxed));
    }

    #[test]
    fn every_tablet_reached_at_once_remembered() {
        let daemon = daemon("state");
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let daemon = &daemon;
                scope.spawn(move || {
                    for i in 0..25 {
                        daemon.reached(&format!("tablet-{thread}-{i}"), format!("10.0.{thread}.{i}:47101").parse().unwrap());
                    }
                });
            }
        });
        daemon.remember_usb("R52X");
        let state = daemon.remembered();
        assert_eq!(state.len(), 8 * 25 + 1);
        assert_eq!(state["tablet-7-24"], "10.0.7.24:47101");
        assert_eq!(state["usb"], json!(["R52X"]));
        let folder = daemon.state.parent().unwrap();
        assert_eq!(std::fs::read_dir(folder).unwrap().count(), 1, "the temporary files left");
    }

    #[test]
    fn a_tablets_paths_are_told_and_listed() {
        let (daemon, tablet) = paired_daemon("paths");
        let (watching, _watched) = app(&daemon);
        assert_eq!(ask(&watching, json!({"op": "watch"}))["ok"], true);
        let device = by_bluetooth(&daemon, &tablet);
        let told = events_until(&watching, &["up", "path"]);
        let path = &told["path"];
        assert_eq!((&path["device"], &path["id"], &path["kind"], &path["transport"], &path["state"]),
                   (&json!(device), &json!(1), &json!("bt"), &json!("stream"), &json!("active")), "{path}");
        let (app, _served) = app(&daemon);
        let devices = ask(&app, json!({"op": "devices"}));
        let listed = &devices["devices"][0]["paths"];
        assert_eq!(listed.as_array().map(Vec::len), Some(1), "{devices}");
        assert_eq!((&listed[0]["id"], &listed[0]["kind"], &listed[0]["state"]), (&json!(1), &json!("bt"), &json!("active")));
        assert!(listed[0]["silent_ms"].is_u64() && listed[0]["budget_from"].is_string(), "{listed}");
        daemon.node.close();
        tablet.close();
    }
}
