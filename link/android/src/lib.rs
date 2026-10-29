//! The link's core in Plus One's app (docs/LINK.md), for LinkCore.kt.
//! A handful of JNI calls, JSON in and out as plus-one-link's socket:
//! - nativeStart(name, certificate, signer): a node, its handle. The key
//!   never leaves Android's keystore: the TLS handshake asks signer.sign;
//! - nativeSetPaired(handle, fingerprints): the PCs it trusts, as they change;
//! - nativeRequest(handle, json): listen, connect, accept_fd (a session the
//!   PC starts on a USB accessory's fd, bridged), accept_stream (on a
//!   Bluetooth socket's, bridged by the app), open (the channel's
//!   descriptor in the answer's "fd", the caller's to own), serve, unserve,
//!   devices, close;
//! - nativeNext(handle, ms): the next event (a channel the PC opened, with
//!   its "fd"; a PC up or gone), or null;
//! - nativeStop(handle).

use jni::objects::{GlobalRef, JByteArray, JObject, JObjectArray, JString, JValue};
use jni::sys::{jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use plusone_link::{Class, Event, Identity, Node};
use rustls::sign::{Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

struct Core {
    node: Node,
    paired: Arc<RwLock<HashSet<String>>>,
    events: Mutex<Receiver<Value>>,
    sender: Sender<Value>,
}

/// The nodes started, by handle: a handle stopped (or never given) is an
/// error, not a crash.
static CORES: LazyLock<Mutex<HashMap<jlong, Arc<Core>>>> = LazyLock::new(Mutex::default);
static NEXT: AtomicI64 = AtomicI64::new(1);
/// Whether the cable has its owner (cable() running).
static CABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The USB accessory, from its opening until the cable is out: sessions on
/// it one after the other. Each: READY and a number, again every
/// USB_AGAIN, until the PC's GO with it; then the session's bytes both
/// ways, until its stream ends (the session over, or on another path);
/// then the next READY. One reader of the accessory throughout: a stale
/// reader never holds it, nor reads another session's bytes.
fn cable(node: Node, file: std::fs::File, kind: &str) {
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    let Ok(mut reader) = file.try_clone() else { return };
    let writer = Arc::new(Mutex::new(file));
    // Where what the PC sends goes: the session's stream, or (none) the handshake.
    let sink: Arc<Mutex<Option<UnixStream>>> = Arc::default();
    let gone = Arc::new(AtomicBool::new(false));
    let (handshake, transfers) = mpsc::channel::<Vec<u8>>();
    {
        let (sink, gone) = (sink.clone(), gone.clone());
        let _ = std::thread::Builder::new().name("link-cable-in".into()).spawn(move || {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,  // the cable out
                    Ok(n) => n,
                };
                let mut sink = sink.lock().unwrap();
                match sink.as_mut() {
                    Some(stream) => {
                        if stream.write_all(&buf[..n]).is_err() {
                            *sink = None;
                        }
                    }
                    None => {
                        let _ = handshake.send(buf[..n].to_vec());
                    }
                }
            }
            gone.store(true, Ordering::SeqCst);
            if let Some(stream) = sink.lock().unwrap().take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
    }
    while !gone.load(Ordering::SeqCst) {
        let number: [u8; 8] = std::array::from_fn(|i| (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_nanos() >> (i * 8)) as u8);
        let mut ready = plusone_link::stream::USB_READY.to_vec();
        ready.extend_from_slice(&number);
        // READY, again until GO: what comes meanwhile (an attempt's of before) dropped.
        let mut said = std::time::Instant::now() - plusone_link::stream::USB_AGAIN;
        let answered = loop {
            if gone.load(Ordering::SeqCst) {
                return;
            }
            if said.elapsed() >= plusone_link::stream::USB_AGAIN {
                let _ = writer.lock().unwrap().write_all(&ready);  // the cable out a moment: again later
                said = std::time::Instant::now();
            }
            match transfers.recv_timeout(Duration::from_millis(200)) {
                Ok(transfer) if plusone_link::stream::usb_number(plusone_link::stream::USB_GO, &transfer) == Some(number) => break true,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break false,
            }
        };
        if !answered {
            return;
        }
        let Ok((ours, theirs)) = UnixStream::pair() else { return };
        plusone_link::stream::little_room(&theirs);
        let Ok(inward) = ours.try_clone() else { return };
        {
            // What came after GO (the PC's first TLS bytes) first, then the rest directly.
            let mut sink = sink.lock().unwrap();
            let mut inward = inward;
            while let Ok(transfer) = transfers.try_recv() {
                let _ = inward.write_all(&transfer);
            }
            *sink = Some(inward);
        }
        let out = {
            let (writer, sink) = (writer.clone(), sink.clone());
            let mut ours = ours;
            std::thread::Builder::new().name("link-cable-out".into()).spawn(move || {
                // Four of the accessory's transfers at once (it queues them), a
                // byte short: a full read goes as one transfer (usb_pieces).
                let mut buf = vec![0u8; (64 << 10) - 1];
                loop {
                    let n = match ours.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut writer = writer.lock().unwrap();
                    if plusone_link::stream::usb_pieces(&buf[..n]).any(|piece| writer.write_all(piece).is_err()) {
                        break;
                    }
                }
                // The session's stream over: the next attempt's.
                if let Some(stream) = sink.lock().unwrap().take() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            })
        };
        let Ok(out) = out else { return };
        match node.accept_stream(Box::new(theirs), kind) {
            Ok(session) => plusone_link::log!("{}: a session by cable", session.name),
            Err(e) => plusone_link::log!("by cable: {e}"),
        }
        let _ = out.join();
    }
}

fn core(handle: jlong) -> Option<Arc<Core>> {
    CORES.lock().unwrap().get(&handle).cloned()
}

// -- signing with the keystore's key --------------------------------------------------

/// The app's key, which Android's keystore keeps: Kotlin's sign(byte[]):
/// byte[] signs (SHA256withECDSA, as TLS 1.3's ecdsa_secp256r1_sha256 wants).
struct Keystore {
    vm: Arc<JavaVM>,
    signer: GlobalRef,
}

impl fmt::Debug for Keystore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keystore")
    }
}

impl SigningKey for Keystore {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(Keystore { vm: self.vm.clone(), signer: self.signer.clone() }) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

fn general(e: impl fmt::Display) -> rustls::Error {
    rustls::Error::General(e.to_string())
}

impl Signer for Keystore {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let mut env = self.vm.attach_current_thread().map_err(general)?;
        let data = env.byte_array_from_slice(message).map_err(general)?;
        let signed = env.call_method(&self.signer, "sign", "([B)[B", &[JValue::Object(&data)])
            .and_then(|value| value.l()).map_err(general)?;
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            return Err(general("the keystore did not sign"));
        }
        env.convert_byte_array(JByteArray::from(signed)).map_err(general)
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

// -- requests ---------------------------------------------------------------------------

fn session_json(session: &plusone_link::Session) -> Value {
    json!({"id": session.peer, "name": session.name, "features": session.features(), "path": session.path(),
           "channels": session.channels(),
           "rtt_ms": session.rtt().map(|r| r.as_secs_f64() * 1000.0), "dropped": session.dropped()})
}

fn request(core: &Arc<Core>, request: &Value) -> Result<Value, String> {
    let text = |key: &str| request[key].as_str().ok_or(format!("no {key}"));
    match request["op"].as_str().unwrap_or("") {
        "listen" => {
            let port = request["port"].as_u64().unwrap_or(0) as u16;
            let bound = core.node.listen(([0, 0, 0, 0], port).into()).map_err(|e| e.to_string())?;
            // QUIC on the same port (UDP): the PC's TCP session moves to it
            // (our hello says it). Without it, TCP alone, as before.
            let quic = match core.node.listen_quic(([0, 0, 0, 0], bound.port()).into()) {
                Ok(quic) => Some(quic.port()),
                Err(e) => {
                    plusone_link::log!("no QUIC at {}: {e}", bound.port());
                    None
                }
            };
            Ok(json!({"ok": true, "port": bound.port(), "quic": quic}))
        }
        "connect" => {
            let address = text("address")?.parse().map_err(|e| format!("the address: {e}"))?;
            let timeout = request["timeout"].as_f64().filter(|t| t.is_finite()).unwrap_or(5.0).clamp(0.1, 60.0);
            let session = core.node.connect(address, Duration::from_secs_f64(timeout))?;
            Ok(json!({"ok": true, "device": session.peer}))
        }
        "accept_fd" => {
            // The cable's for as long as it is plugged in: one owner, which
            // opened it once (Android's accessory opens only once).
            let fd = request["fd"].as_i64().filter(|fd| *fd >= 0).ok_or("no fd")? as i32;
            let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            if CABLE.swap(true, Ordering::SeqCst) {
                return Err("the cable has an owner already".into());
            }
            let (node, kind) = (core.node.clone(), text("path").unwrap_or("usb").to_owned());
            std::thread::Builder::new().name("link-cable".into()).spawn(move || {
                cable(node, file, &kind);
                CABLE.store(false, Ordering::SeqCst);
            }).map_err(|e| e.to_string())?;
            Ok(json!({"ok": true}))
        }
        "accept_stream" => {
            // A stream the app made (a Bluetooth socket's, bridged): a path
            // of that kind, the PC making the session on it.
            let fd = request["fd"].as_i64().filter(|fd| *fd >= 0).ok_or("no fd")? as i32;
            let stream = std::os::unix::net::UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let (node, kind) = (core.node.clone(), text("path")?.to_owned());
            std::thread::Builder::new().name("link-stream".into()).spawn(move || {
                match node.accept_stream(Box::new(stream), &kind) {
                    Ok(session) => plusone_link::log!("{}: a session by {kind}", session.name),
                    Err(e) => plusone_link::log!("by {kind}: {e}"),
                }
            }).map_err(|e| e.to_string())?;
            Ok(json!({"ok": true}))
        }
        "open" => {
            let session = core.node.session(text("device")?).ok_or("not connected")?;
            let class = request["class"].as_str().and_then(Class::parse).ok_or("no class")?;
            let channel = session.open(text("feature")?, request["channel"].as_str().unwrap_or(""), class)?;
            let id = channel.id;
            Ok(json!({"ok": true, "id": id, "fd": channel.into_fd().into_raw_fd()}))
        }
        "serve" => {
            let feature = text("feature")?.to_owned();
            let sender = Mutex::new(core.sender.clone());
            core.node.serve(&feature, request["version"].as_u64().unwrap_or(1) as u32, move |incoming| {
                let channel = incoming.channel;
                let event = json!({"event": "incoming", "device": incoming.session.peer, "feature": channel.feature,
                                   "channel": channel.name, "class": channel.class.name(), "id": channel.id});
                let mut event = event;
                event["fd"] = json!(channel.into_fd().into_raw_fd());
                let _ = sender.lock().unwrap().send(event);
            });
            Ok(json!({"ok": true}))
        }
        "unserve" => {
            core.node.unserve(text("feature")?);
            Ok(json!({"ok": true}))
        }
        "devices" => Ok(json!({"ok": true, "devices": core.node.sessions().iter().map(|s| session_json(s)).collect::<Vec<_>>()})),
        "close" => {
            if let Some(session) = core.node.session(text("device")?) {
                session.close();
            }
            Ok(json!({"ok": true}))
        }
        other => Err(format!("no such request: {other:?}")),
    }
}

// -- JNI ------------------------------------------------------------------------------

fn string(env: &mut JNIEnv, value: &JString) -> String {
    env.get_string(value).map(|s| s.into()).unwrap_or_default()
}

fn new_string(env: &mut JNIEnv, value: &str) -> jstring {
    env.new_string(value).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeStart<'local>(
    mut env: JNIEnv<'local>, _this: JObject<'local>, name: JString<'local>, certificate: JByteArray<'local>,
    signer: JObject<'local>) -> jlong {
    let name = string(&mut env, &name);
    let Ok(certificate) = env.convert_byte_array(&certificate) else { return 0 };
    let (Ok(vm), Ok(signer)) = (env.get_java_vm(), env.new_global_ref(signer)) else { return 0 };
    let identity = Identity::signed_by(certificate, Arc::new(Keystore { vm: Arc::new(vm), signer }));
    let paired: Arc<RwLock<HashSet<String>>> = Arc::default();
    let trusted = paired.clone();
    let Ok(node) = Node::new(&name, &identity, Arc::new(move |fp: &str| trusted.read().unwrap().contains(fp))) else { return 0 };
    let (sender, events) = mpsc::channel();
    let on_event = Mutex::new(sender.clone());
    node.on_event(move |event| {
        let message = match event {
            Event::Up(session) => {
                let mut message = session_json(&session);
                message["event"] = json!("up");
                message["device"] = json!(session.peer);
                message
            }
            Event::Moved(session) => json!({"event": "moved", "device": session.peer, "path": session.path()}),
            Event::Budget(session, rate) => json!({"event": "budget", "device": session.peer, "kbps": rate * 8 / 1000}),
            Event::Detached(session) => json!({"event": "detached", "device": session.peer}),
            Event::Gone(device, reason) => json!({"event": "gone", "device": device, "reason": reason}),
        };
        let _ = on_event.lock().unwrap().send(message);
    });
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    CORES.lock().unwrap().insert(handle, Arc::new(Core { node, paired, events: Mutex::new(events), sender }));
    handle
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeSetPaired<'local>(
    mut env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong, fingerprints: JObjectArray<'local>) {
    let Some(core) = core(handle) else { return };
    let count = env.get_array_length(&fingerprints).unwrap_or(0);
    let mut paired = HashSet::new();
    for i in 0..count {
        if let Ok(item) = env.get_object_array_element(&fingerprints, i) {
            paired.insert(string(&mut env, &JString::from(item)));
        }
    }
    *core.paired.write().unwrap() = paired;
    // Forgotten by the user: its sessions over at once.
    core.node.check_paired();
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeRequest<'local>(
    mut env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong, json: JString<'local>) -> jstring {
    let text = string(&mut env, &json);
    let reply = match (core(handle), serde_json::from_str::<Value>(&text)) {
        (None, _) => json!({"ok": false, "error": "stopped"}),
        (_, Err(e)) => json!({"ok": false, "error": e.to_string()}),
        (Some(core), Ok(message)) => request(&core, &message).unwrap_or_else(|e| json!({"ok": false, "error": e})),
    };
    new_string(&mut env, &reply.to_string())
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeNext<'local>(
    mut env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong, timeout_ms: jint) -> jstring {
    let Some(core) = core(handle) else { return std::ptr::null_mut() };
    let event = core.events.lock().unwrap().recv_timeout(Duration::from_millis(timeout_ms.max(0) as u64));
    match event {
        Ok(event) => new_string(&mut env, &event.to_string()),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeStop<'local>(
    _env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong) {
    if let Some(core) = CORES.lock().unwrap().remove(&handle) {
        core.node.close();
    }
}
