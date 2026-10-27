//! plus-one-link: Plus One's link as a program of its own (docs/LINK.md).
//! It keeps the sessions with the paired tablets; apps (Plus One's
//! features, in any language) ask it for channels through its socket.
//!
//!   plus-one-link [--socket PATH] [--config DIR] [--name NAME] [--listen ADDRESS]
//!
//! The socket: $XDG_RUNTIME_DIR/plus-one/link (or systemd's). The identity
//! and the paired tablets: what Plus One keeps in ~/.config/plus-one/companion
//! (cert.pem, key.pem, paired.json), read as they are.
//!
//! Requests, each answered {"ok": true, ...} or {"ok": false, "error"}:
//!   {"op": "devices"}            the paired tablets: id, name, connected,
//!                                features, rtt_ms, dropped
//!   {"op": "connect", "address": "IP:PORT"[, "timeout": S]}
//!                                a session with the tablet there: its id
//!   {"op": "open", "device", "feature", "channel", "class"}
//!                                a channel, its descriptor with the answer
//!   {"op": "serve", "feature"[, "version"]}
//!                                the tablet's channels for this feature, from
//!                                then on, as {"event": "incoming", "device",
//!                                "feature", "channel", "class", "id"} with
//!                                each one's descriptor; until this
//!                                connection closes
//!   {"op": "watch"}              {"event": "up"|"gone", "device", ...} from
//!                                then on
//! A connection that served or watched carries only events after that.

mod api;

use plusone_link::{log, Class, Event, Identity, Node};
use serde_json::{json, Value};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REQUEST: usize = 64 << 10;

struct Daemon {
    node: Node,
    config: PathBuf,
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
                "dropped": session.map(|s| s.dropped()).unwrap_or(0),
            }));
        }
        json!({"ok": true, "devices": devices})
    }

    fn connect(&self, request: &Value) -> Result<Value, String> {
        let address = request["address"].as_str().ok_or("no address")?
            .parse().map_err(|e| format!("the address: {e}"))?;
        let timeout = Duration::from_secs_f64(request["timeout"].as_f64().unwrap_or(5.0).clamp(0.1, 60.0));
        let session = self.node.connect(address, timeout)?;
        Ok(json!({"ok": true, "device": session.peer, "name": session.name}))
    }

    fn open(&self, request: &Value) -> Result<(Value, OwnedFd), String> {
        let device = request["device"].as_str().ok_or("no device")?;
        let session = self.node.session(device).ok_or("not connected")?;
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

    fn event(&self, event: Event) {
        let message = match event {
            Event::Up(session) => json!({"event": "up", "device": session.peer, "name": session.name,
                                         "features": session.features()}),
            Event::Gone(device, reason) => json!({"event": "gone", "device": device, "reason": reason}),
        };
        self.watchers.lock().unwrap().retain(|watcher| api::send(watcher.as_raw_fd(), &message, None).is_ok());
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
    let daemon = Arc::new(Daemon { node: node.clone(), config, watchers: Mutex::default(), serving: Mutex::default(),
                                   serial: AtomicU64::new(0) });
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
