//! What a session leaves behind, and how often its threads wake: counted
//! for the whole process, so in a process of its own, one test at a time.

use plusone_link::{Class, Identity, Node, Paired};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
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

/// How many times the threads named so went to sleep, all told.
fn sleeps(name: &str) -> u64 {
    threads(name).iter().map(|task| {
        let status = std::fs::read_to_string(task.join("status")).unwrap_or_default();
        status.lines().find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|count| count.trim().parse::<u64>().ok()).unwrap_or(0)
    }).sum()
}

fn descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// A TCP proxy to address (its own address), and what cuts it: both of its
/// connections shut down, as when the network is lost.
fn proxy(address: SocketAddr) -> (SocketAddr, impl FnOnce()) {
    use std::net::{Shutdown, TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let via = listener.local_addr().unwrap();
    let joined = std::thread::spawn(move || {
        let (near, _) = listener.accept().unwrap();
        let far = TcpStream::connect(address).unwrap();
        for (mut from, mut to) in [(near.try_clone().unwrap(), far.try_clone().unwrap()),
                                   (far.try_clone().unwrap(), near.try_clone().unwrap())] {
            std::thread::spawn(move || std::io::copy(&mut from, &mut to));
        }
        (near, far)
    });
    let cut = move || {
        let (near, far) = joined.join().unwrap();
        let _ = near.shutdown(Shutdown::Both);
        let _ = far.shutdown(Shutdown::Both);
    };
    (via, cut)
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

#[test]
fn a_channel_waiting_for_credit_sleeps_until_it_comes() {
    // A tablet's feature that reads nothing for a while: the PC's side of
    // its channel stops at the window, and sleeps meanwhile (it looked
    // again each 2 ms, about 500 times a second); read, all goes.
    let _one = one_at_a_time();
    let (pc, tablet, address) = pair();
    let (read, reading) = mpsc::channel::<()>();
    let reading = Mutex::new(reading);
    let (got, total) = mpsc::channel();
    tablet.serve("deaf", 1, move |incoming| {
        let _ = reading.lock().unwrap().recv();
        let mut buf = vec![0u8; 1 << 20];
        let mut bytes = 0;
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
            bytes += n;
        }
        let _ = got.send(bytes);
    });
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let channel = session.open("deaf", "x", Class::Control).unwrap();
    let sending = std::thread::spawn(move || {
        let chunk = vec![7u8; 200 << 10];
        for _ in 0..20 {  // 4 MB: past the window (1 MiB) and what the tablet's socket holds
            channel.send(&chunk).unwrap();
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    let before = sleeps("link-channel");
    std::thread::sleep(Duration::from_secs(1));
    let woke = sleeps("link-channel") - before;
    read.send(()).unwrap();
    sending.join().unwrap();
    assert_eq!(total.recv_timeout(Duration::from_secs(10)).unwrap(), 20 * (200 << 10));
    assert!(woke <= 2, "the channels' threads woke {woke} times in a second, waiting for credit");
    pc.close();
    tablet.close();
}

#[test]
fn a_channel_waiting_for_room_while_away_sleeps_until_a_path_comes() {
    // The path lost, a control message past CONTROL_BACKLOG waits for the
    // one before it to go: asleep (it looked again each 2 ms), and on its
    // way as soon as a path is back.
    let _one = one_at_a_time();
    let (pc, tablet, address) = pair();
    let (via, cut) = proxy(address);
    let session = pc.connect(via, Duration::from_secs(2)).unwrap();
    let channel = session.open("echo", "c", Class::Control).unwrap();
    cut();
    let started = Instant::now();
    while session.path().is_some() {
        assert!(started.elapsed() < Duration::from_secs(5), "the path never lost");
        std::thread::sleep(Duration::from_millis(20));
    }
    let chunk = vec![7u8; 200 << 10];
    channel.send(&chunk).unwrap();
    channel.send(&chunk).unwrap();  // waits for room behind the first
    std::thread::sleep(Duration::from_millis(300));
    let before = sleeps("link-channel");
    std::thread::sleep(Duration::from_secs(1));
    let woke = sleeps("link-channel") - before;
    assert!(woke <= 2, "the channels' threads woke {woke} times in a second, waiting for room");
    let again = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert!(Arc::ptr_eq(&again, &session), "the same session, another path");
    let mut buf = vec![0u8; 1 << 20];
    for _ in 0..2 {
        assert_eq!(channel.recv(&mut buf, WAIT).unwrap(), Some(chunk.len()));
    }
    pc.close();
    tablet.close();
}
