//! What a session leaves behind, and how often its threads wake: counted
//! for the whole process, so in a process of its own, one test at a time.

use plusone_link::{Class, Identity, Node, Paired};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const WAIT: Option<Duration> = Some(Duration::from_secs(5));

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn identity() -> (Identity, String) {
    let made = rcgen::generate_simple_self_signed(vec!["plus-one".into()]).unwrap();
    let identity = Identity::new(made.cert.der().to_vec(), made.key_pair.serialize_der()).unwrap();
    let fingerprint = identity.fingerprint();
    (identity, fingerprint)
}

fn trusting(fingerprint: &str) -> Paired {
    let fingerprint = fingerprint.to_owned();
    Arc::new(move |seen: &str| seen == fingerprint)
}

/// A tablet that echoes every message on "echo" channels, and a PC; the
/// tablet's address.
fn pair() -> (Node, Node, SocketAddr) {
    let (pc_id, pc_fp) = identity();
    let (tablet_id, tablet_fp) = identity();
    let tablet = Node::new("tablet", &tablet_id, trusting(&pc_fp)).unwrap();
    tablet.serve("echo", 1, |incoming| {
        let mut buf = vec![0u8; 1 << 20];
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 || incoming.channel.send(&buf[..n]).is_err() {
                break;
            }
        }
    });
    let address = tablet.listen("127.0.0.1:0".parse().unwrap()).unwrap();
    let pc = Node::new("pc", &pc_id, trusting(&tablet_fp)).unwrap();
    (pc, tablet, address)
}

/// The process's threads whose names start so (the link's: "link-"), each
/// one's directory in /proc.
fn threads(prefix: &str) -> Vec<PathBuf> {
    std::fs::read_dir("/proc/self/task").unwrap().flatten().map(|task| task.path())
        .filter(|task| std::fs::read_to_string(task.join("comm")).is_ok_and(|name| name.trim_end().starts_with(prefix)))
        .collect()
}

fn descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// The link's threads and the process's descriptors, once back to at most
/// was (a session's take a moment to go), or as they are 5 s on.
fn back_to(was: (usize, usize)) -> (usize, usize) {
    let started = Instant::now();
    loop {
        let now = (threads("link-").len(), descriptors());
        if (now.0 <= was.0 && now.1 <= was.1) || started.elapsed() > Duration::from_secs(5) {
            return now;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_session_over_leaves_no_thread_or_socket_behind() {
    // Each control channel open as the session ended kept a thread (its
    // inbox's) and its socket for good, on both sides.
    let _one = one_at_a_time();
    let (pc, tablet, address) = pair();
    std::thread::sleep(Duration::from_millis(100));  // the listener's thread named
    let before = (threads("link-").len(), descriptors());
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let mut buf = [0u8; 64];
    let channels: Vec<_> = (0..5).map(|i| {
        let channel = session.open("echo", &format!("c{i}"), Class::Control).unwrap();
        channel.send(b"up").unwrap();
        assert_eq!(channel.recv(&mut buf, WAIT).unwrap(), Some(2));
        channel
    }).collect();
    session.close();
    drop((channels, session));
    let after = back_to(before);
    assert!(after.0 <= before.0 && after.1 <= before.1,
            "the link's threads and the descriptors: {before:?} before the session, {after:?} after it");
    pc.close();
    tablet.close();
}

#[test]
fn an_open_refused_leaves_no_thread_or_socket_behind() {
    // Each control channel refused (or unanswered) kept its inbox's thread
    // and its socket, as long as the session lasted.
    let _one = one_at_a_time();
    let (pc, tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    // Every thread of the session's up first, on both sides (UDP too).
    let started = Instant::now();
    while session.rtt().is_none() || tablet.sessions().first().and_then(|s| s.rtt()).is_none() {
        assert!(started.elapsed() < Duration::from_secs(5), "UDP never proven");
        std::thread::sleep(Duration::from_millis(20));
    }
    let before = (threads("link-").len(), descriptors());
    for _ in 0..20 {
        assert!(session.open("not served", "x", Class::Control).is_err());
    }
    let after = back_to(before);
    assert!(after.0 <= before.0 && after.1 <= before.1,
            "the link's threads and the descriptors: {before:?} before 20 opens refused, {after:?} after them");
    pc.close();
    tablet.close();
}
