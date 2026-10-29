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
//!   its "fd"; a PC up, moved, gone: "path" and "transport" with up and
//!   moved), or null;
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

/// The cable's ownership, taken (CABLE): given back as it is dropped,
/// however its owner ends (its thread not made, a panic).
struct CableOwner;

impl CableOwner {
    fn take() -> Option<CableOwner> {
        (!CABLE.swap(true, Ordering::SeqCst)).then_some(CableOwner)
    }
}

impl Drop for CableOwner {
    fn drop(&mut self) {
        CABLE.store(false, Ordering::SeqCst);
    }
}

/// The USB accessory, from its opening until the cable is out, or its node
/// closed (the app's service stopped): sessions on it one after the other.
/// Each: READY and a number, again every USB_AGAIN, until the PC's GO with
/// it; then the session's bytes both ways, until its stream ends (the
/// session over, or on another path); then the next READY. One reader of
/// the accessory throughout: a stale reader never holds it, nor reads
/// another session's bytes. Its read cannot be woken but by the PC's bytes:
/// with the node closed, READY still goes, for the PC's answer to end it,
/// and no session is made; the accessory is let go, for the next service
/// in this process to open it.
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
        let (sink, gone, node) = (sink.clone(), gone.clone(), node.clone());
        let _ = std::thread::Builder::new().name("link-cable-in".into()).spawn(move || {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,  // the cable out
                    Ok(_) if node.is_closed() => break,
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
        if !answered || node.is_closed() {
            return;  // closed: the reader ends on the PC's next bytes (its TLS)
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
        let signed = keystore_sign(&mut env, &self.signer, message);
        // An exception (Kotlin's, or JNI's own) told and cleared, whatever
        // failed: left pending, the thread's detach would end the app.
        if env.exception_check().unwrap_or(true) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
            return Err(general("the keystore did not sign"));
        }
        signed.map_err(general)
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

/// signer.sign(message): the signature, or an error (its null: it could not).
fn keystore_sign(env: &mut JNIEnv, signer: &GlobalRef, message: &[u8]) -> jni::errors::Result<Vec<u8>> {
    let data = env.byte_array_from_slice(message)?;
    let signed = env.call_method(signer, "sign", "([B)[B", &[JValue::Object(&data)])?.l()?;
    env.convert_byte_array(JByteArray::from(signed))
}

// -- requests ---------------------------------------------------------------------------

/// request[key] as the number it is used as: one out of its range an
/// error, never another number; default when it is not there.
fn number<T: TryFrom<u64>>(request: &Value, key: &str, default: Option<T>) -> Result<T, String> {
    match request.get(key) {
        None | Some(Value::Null) => default.ok_or_else(|| format!("no {key}")),
        Some(value) => value.as_u64().and_then(|n| T::try_from(n).ok()).ok_or_else(|| format!("{key} out of range: {value}")),
    }
}

fn session_json(session: &plusone_link::Session) -> Value {
    json!({"id": session.peer, "name": session.name, "features": session.features(), "path": session.path(),
           "transport": session.transport(), "channels": session.channels(),
           "rtt_ms": session.rtt().map(|r| r.as_secs_f64() * 1000.0), "dropped": session.dropped()})
}

fn request(core: &Arc<Core>, request: &Value) -> Result<Value, String> {
    let text = |key: &str| request[key].as_str().ok_or(format!("no {key}"));
    match request["op"].as_str().unwrap_or("") {
        "listen" => {
            let port: u16 = number(request, "port", Some(0))?;
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
            let fd: i32 = number(request, "fd", None)?;
            let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let owner = CableOwner::take().ok_or("the cable has an owner already")?;
            let (node, kind) = (core.node.clone(), text("path").unwrap_or("usb").to_owned());
            std::thread::Builder::new().name("link-cable".into()).spawn(move || {
                let _owner = owner;
                cable(node, file, &kind);
            }).map_err(|e| e.to_string())?;
            Ok(json!({"ok": true}))
        }
        "accept_stream" => {
            // A stream the app made (a Bluetooth socket's, bridged): a path
            // of that kind, the PC making the session on it.
            let fd: i32 = number(request, "fd", None)?;
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
            let version: u32 = number(request, "version", Some(1))?;
            let sender = Mutex::new(core.sender.clone());
            core.node.serve(&feature, version, move |incoming| {
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

/// body's value, or failed()'s if it panicked: a panic unwinding out of an
/// extern "system" call ends the app (Rust aborts there), and one thread
/// of the core's that panicked holding a lock (a session's) would make
/// each call after it panic too. What the app is told is a failure it
/// handles already: an error answer, no event, no node.
fn guarded<T>(body: impl FnOnce() -> T, failed: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or_else(|panic| {
        let what = panic.downcast_ref::<&str>().copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("?");
        plusone_link::log!("a panic under a call of the app's ({what}): answered as a failure");
        failed()
    })
}

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
    guarded(|| start(&mut env, name, certificate, signer), || 0)
}

fn start(env: &mut JNIEnv, name: JString, certificate: JByteArray, signer: JObject) -> jlong {
    let name = string(env, &name);
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
            // transport: "quic", "tcp", "stream" (Session::transport); a TCP
            // path moved to QUIC is a "moved" at the same path.
            Event::Moved(session) => json!({"event": "moved", "device": session.peer, "path": session.path(),
                                            "transport": session.transport()}),
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
    guarded(|| {
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
    }, || ())
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeRequest<'local>(
    mut env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong, json: JString<'local>) -> jstring {
    let text = string(&mut env, &json);
    let reply = guarded(|| match (core(handle), serde_json::from_str::<Value>(&text)) {
        (None, _) => json!({"ok": false, "error": "stopped"}),
        (_, Err(e)) => json!({"ok": false, "error": e.to_string()}),
        (Some(core), Ok(message)) => request(&core, &message).unwrap_or_else(|e| json!({"ok": false, "error": e})),
    }, || json!({"ok": false, "error": "the link's core failed"}));
    new_string(&mut env, &reply.to_string())
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeNext<'local>(
    mut env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong, timeout_ms: jint) -> jstring {
    let wait = Duration::from_millis(timeout_ms.max(0) as u64);
    let event = guarded(|| {
        let core = core(handle)?;
        let event = core.events.lock().unwrap().recv_timeout(wait);
        event.ok()
    }, || {
        std::thread::sleep(wait);  // as if nothing came: the app's events thread asks again then, not at once
        None
    });
    match event {
        Some(event) => new_string(&mut env, &event.to_string()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_github_gabrielesantangelo_plusone_link_LinkCore_nativeStop<'local>(
    _env: JNIEnv<'local>, _this: JObject<'local>, handle: jlong) {
    guarded(|| {
        // Out of CORES first: its lock not held while the node closes.
        let core = CORES.lock().unwrap().remove(&handle);
        if let Some(core) = core {
            core.node.close();
        }
    }, || ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A key that signs nothing: no handshake gets that far here.
    #[derive(Debug)]
    struct NoKey;

    impl SigningKey for NoKey {
        fn choose_scheme(&self, _offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
            None
        }

        fn algorithm(&self) -> SignatureAlgorithm {
            SignatureAlgorithm::ECDSA
        }
    }

    fn node() -> Node {
        let identity = Identity::signed_by(b"a tablet".to_vec(), Arc::new(NoKey));
        Node::new("tablet", &identity, Arc::new(|_: &str| false)).unwrap()
    }

    /// The PC on the accessory's other end: the app's READY answered with GO.
    fn go(pc: &mut UnixStream) {
        let mut buf = [0u8; 256];
        let n = pc.read(&mut buf).unwrap();
        let number = plusone_link::stream::usb_number(plusone_link::stream::USB_READY, &buf[..n]).unwrap();
        let mut go = plusone_link::stream::USB_GO.to_vec();
        go.extend_from_slice(&number);
        pc.write_all(&go).unwrap();
    }

    #[test]
    fn the_cable_lets_the_accessory_go_once_its_node_is_closed() {
        // The service stopped (nativeStop): the next one in this process opens the accessory again.
        let (accessory, mut pc) = UnixStream::pair().unwrap();
        let node = node();
        let cable_node = node.clone();
        let cable = std::thread::spawn(move || cable(cable_node, std::fs::File::from(OwnedFd::from(accessory)), "usb"));
        node.close();
        go(&mut pc);
        // No session tried on the closed node (a handshake, the keystore's
        // signature): each of the cable's descriptors closed instead.
        pc.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 256];
        assert_eq!(pc.read(&mut buf).map_err(|e| e.kind()), Ok(0));
        cable.join().unwrap();
    }

    #[test]
    fn a_panic_under_a_call_is_its_failure_not_the_apps_end() {
        // A lock of the core's (a session's) poisoned by one of its threads
        // that panicked holding it: whoever takes it next panics too, as
        // "devices" would under nativeRequest.
        let lock = Arc::new(Mutex::new(0u32));
        let held = lock.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a thread of the core's fails holding it");
        }).join();
        let reply = guarded(|| json!({"ok": true, "channels": *lock.lock().unwrap()}),
                            || json!({"ok": false, "error": "the link's core failed"}));
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(guarded(|| json!({"ok": true}), || json!(null))["ok"], json!(true));
    }

    #[test]
    fn the_cables_owner_is_given_back_however_it_ends() {
        let owner = CableOwner::take().unwrap();
        assert!(CableOwner::take().is_none());
        // Its thread not made: the closure that held it dropped, as a panic would.
        let _ = std::panic::catch_unwind(move || {
            let _owner = owner;
            panic!("the cable's thread failed");
        });
        assert!(CableOwner::take().is_some());
    }

    #[test]
    fn a_number_out_of_its_range_is_an_error_not_another() {
        let request = json!({"port": 65536, "fd": 2147483648u64, "version": 4294967296u64, "negative": -1});
        assert!(number::<u16>(&request, "port", Some(0)).is_err());
        assert!(number::<i32>(&request, "fd", None).is_err());
        assert!(number::<u32>(&request, "version", Some(1)).is_err());
        assert!(number::<i32>(&request, "negative", None).is_err());
        assert!(number::<u16>(&json!({"port": "47236"}), "port", Some(0)).is_err());
    }

    #[test]
    fn a_number_in_its_range_or_its_default() {
        let request = json!({"port": 65535, "fd": 2147483647, "version": 3});
        assert_eq!(number::<u16>(&request, "port", Some(0)), Ok(65535));
        assert_eq!(number::<i32>(&request, "fd", None), Ok(i32::MAX));
        assert_eq!(number::<u32>(&request, "version", Some(1)), Ok(3));
        assert_eq!(number::<u16>(&json!({}), "port", Some(0)), Ok(0));
        assert_eq!(number::<u32>(&json!({"version": null}), "version", Some(1)), Ok(1));
        assert_eq!(number::<i32>(&json!({}), "fd", None), Err("no fd".to_owned()));
    }
}
