//! Two nodes on loopback: a PC and a tablet, paired with each other.

use plusone_link::{Class, Event, Identity, Node, Paired};
use std::net::SocketAddr;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const WAIT: Option<Duration> = Some(Duration::from_secs(5));

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

#[test]
fn hello_says_names_and_features() {
    let (pc, _tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert_eq!(session.name, "tablet");
    assert_eq!(session.features()["echo"], 1);
}

#[test]
fn features_served_later_are_told() {
    let (pc, tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    tablet.serve("speakers", 3, |_| {});
    let started = Instant::now();
    while session.features()["speakers"] != 3 {
        assert!(started.elapsed() < Duration::from_secs(2), "never told");
        std::thread::sleep(Duration::from_millis(10));
    }
    tablet.unserve("speakers");
    std::thread::sleep(Duration::from_millis(200));
    assert!(session.features().get("speakers").is_none());
    assert!(session.open("speakers", "pcm", Class::Realtime).is_err());
}

#[test]
fn control_messages_both_ways_in_order() {
    let (pc, _tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let channel = session.open("echo", "test", Class::Control).unwrap();
    let big = vec![7u8; 200 << 10];
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..50u8 {
        let message: &[u8] = if i == 25 { &big } else { std::slice::from_ref(&i) };
        channel.send(message).unwrap();
        let n = channel.recv(&mut buf, WAIT).unwrap().unwrap();
        assert_eq!(&buf[..n], message);
    }
}

#[test]
fn realtime_goes_over_udp_once_proven() {
    let (pc, _tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let channel = session.open("echo", "video", Class::Realtime).unwrap();
    let mut buf = [0u8; 2048];
    // At once, before any pong: over the stream.
    channel.send(b"early").unwrap();
    assert_eq!(channel.recv(&mut buf, WAIT).unwrap(), Some(5));
    let started = Instant::now();
    while session.rtt().is_none() {
        assert!(started.elapsed() < Duration::from_secs(2), "UDP never proven");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut got = 0;
    for i in 0..500u32 {
        let mut packet = vec![0u8; 1200];
        packet[..4].copy_from_slice(&i.to_be_bytes());
        channel.send(&packet).unwrap();
        if let Some(n) = channel.recv(&mut buf, Some(Duration::from_millis(200))).unwrap() {
            assert_eq!(n, 1200);
            got += 1;
        }
    }
    assert!(got >= 495, "{got} of 500 came back");
    // Too big for a datagram: dropped, counted.
    channel.send(&[0u8; 1423]).unwrap();
    assert_eq!(channel.recv(&mut buf, Some(Duration::from_millis(300))).unwrap(), None);
    assert!(session.dropped() >= 1);
}

#[test]
fn a_feature_not_served_is_refused() {
    let (pc, _tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let error = session.open("camera", "video", Class::Realtime).err().unwrap();
    assert!(error.contains("not served"), "{error}");
}

#[test]
fn an_unpaired_device_gets_no_session() {
    let (_pc, _tablet, address) = pair();
    let (stranger_id, _) = identity();
    let stranger = Node::new("stranger", &stranger_id, Arc::new(|_: &str| true)).unwrap();
    assert!(stranger.connect(address, Duration::from_secs(2)).is_err());
}

#[test]
fn closing_a_channel_ends_it_on_the_other_side() {
    let (pc, tablet, address) = pair();
    let (opened, heard) = mpsc::channel();
    tablet.serve("sink", 1, move |incoming| {
        let mut buf = [0u8; 64];
        let mut ended = false;
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, WAIT) {
            if n == 0 {
                ended = true;
                break;
            }
        }
        opened.send(ended).unwrap();
    });
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let channel = session.open("sink", "x", Class::Control).unwrap();
    channel.send(b"one").unwrap();
    drop(channel);
    assert_eq!(heard.recv_timeout(Duration::from_secs(5)), Ok(true));
}

#[test]
fn a_session_closed_ends_its_channels_and_says_so() {
    let (pc, _tablet, address) = pair();
    let (events, gone) = mpsc::channel();
    pc.on_event(move |event| {
        if let Event::Gone(peer, reason) = event {
            let _ = events.send((peer, reason));
        }
    });
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let channel = session.open("echo", "x", Class::Realtime).unwrap();
    session.close();
    let mut buf = [0u8; 64];
    assert_eq!(channel.recv(&mut buf, WAIT).unwrap(), Some(0));
    let (peer, _) = gone.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(peer, session.peer);
    assert!(pc.session(&session.peer).is_none());
}

#[test]
fn the_tablet_hears_the_pc_go() {
    let (pc, tablet, address) = pair();
    let (events, gone) = mpsc::channel();
    tablet.on_event(move |event| {
        if let Event::Gone(_, reason) = event {
            let _ = events.send(reason);
        }
    });
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(tablet.sessions().len(), 1);
    session.close();
    gone.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(tablet.sessions().is_empty());
}

#[test]
fn a_second_connection_goes_on_with_the_session() {
    let (pc, tablet, address) = pair();
    let first = pc.connect(address, Duration::from_secs(2)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let second = pc.connect(address, Duration::from_secs(2)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(Arc::ptr_eq(&first, &second), "the same session");
    assert!(!first.is_closed());
    assert_eq!(tablet.sessions().len(), 1);
}

fn until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(8), "{what}: not in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_session_goes_on_over_another_path() {
    let (pc, tablet, address) = pair();
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let cable = a.try_clone().unwrap();
    let accepter = tablet.clone();
    let accepting = std::thread::spawn(move || accepter.accept_stream(Box::new(b), "usb").map(|s| s.id.clone()));
    let session = pc.connect_stream(Box::new(a), "usb").unwrap();
    assert_eq!(accepting.join().unwrap().unwrap(), session.id);
    assert_eq!(session.path().as_deref(), Some("usb"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let video = session.open("echo", "v", Class::Realtime).unwrap();
    let mut buf = vec![0u8; 4096];
    control.send(b"by cable").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(8));

    // The cable pulled: the session waits; what is said meanwhile waits too.
    cable.shutdown(std::net::Shutdown::Both).unwrap();
    until("the path lost", || session.path().is_none());
    control.send(b"while away").unwrap();
    let again = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert!(Arc::ptr_eq(&again, &session), "the same session, another path");
    until("on the network", || session.path().as_deref() == Some("lan"));
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(10));
    assert_eq!(&buf[..10], b"while away");
    video.send(&[7u8; 1000]).unwrap();
    assert_eq!(video.recv(&mut buf, WAIT).unwrap(), Some(1000));
    assert!(!session.is_closed());
    assert_eq!(tablet.sessions().len(), 1);
}

#[test]
fn a_better_path_takes_over() {
    let (pc, tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert_eq!(session.path().as_deref(), Some("lan"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let accepter = tablet.clone();
    std::thread::spawn(move || accepter.accept_stream(Box::new(b), "usb").map(|_| ()));
    let by_cable = pc.connect_stream(Box::new(a), "usb").unwrap();
    assert!(Arc::ptr_eq(&by_cable, &session));
    until("on the cable", || session.path().as_deref() == Some("usb"));
    let mut buf = vec![0u8; 64];
    control.send(b"moved").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(5));
}

#[test]
fn a_node_closed_listens_no_more() {
    let (pc, tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    tablet.close();
    let started = Instant::now();
    while !session.is_closed() {
        assert!(started.elapsed() < Duration::from_secs(2), "the session went on");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(pc.connect(address, Duration::from_secs(1)).is_err());
}

#[test]
fn a_session_on_a_stream_with_no_ip_carries_everything() {
    // As over USB or Bluetooth: one byte stream, no UDP.
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
    let pc = Node::new("pc", &pc_id, trusting(&tablet_fp)).unwrap();
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let accepter = tablet.clone();
    let accepting = std::thread::spawn(move || accepter.accept_stream(Box::new(b), "usb").map(|s| s.path()));
    let session = pc.connect_stream(Box::new(a), "usb").unwrap();
    assert_eq!(accepting.join().unwrap().unwrap().as_deref(), Some("usb"));
    assert_eq!(session.path().as_deref(), Some("usb"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let video = session.open("echo", "v", Class::Realtime).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    control.send(b"control").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(7));
    let mut got = 0;
    for i in 0..300u32 {
        let mut packet = vec![0u8; 1422];
        packet[..4].copy_from_slice(&i.to_be_bytes());
        video.send(&packet).unwrap();
        if video.recv(&mut buf, Some(Duration::from_millis(200))).unwrap() == Some(1422) {
            got += 1;
        }
    }
    assert_eq!(got, 300);
    std::thread::sleep(Duration::from_millis(1200));
    assert!(session.rtt().is_none(), "no UDP to measure");
    assert!(!session.is_closed());
}

#[test]
fn a_path_the_other_side_lost_is_replaced_even_by_a_worse_one() {
    // The cable dies on the PC's side only: the tablet goes on believing in
    // it (nothing tells it until its silence). The PC's Wi-Fi path must be
    // taken at once, not refused as worse than the cable.
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let (pc, tablet, address) = pair();
    let (pc_end, relay_pc) = UnixStream::pair().unwrap();
    let (tablet_end, relay_tablet) = UnixStream::pair().unwrap();
    let frozen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (mut from, mut to) in [(relay_pc.try_clone().unwrap(), relay_tablet.try_clone().unwrap()),
                               (relay_tablet.try_clone().unwrap(), relay_pc.try_clone().unwrap())] {
        let frozen = frozen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            while let Ok(n) = from.read(&mut buf) {
                if n == 0 || frozen.load(std::sync::atomic::Ordering::SeqCst) || to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        });
    }
    let accepter = tablet.clone();
    std::thread::spawn(move || accepter.accept_stream(Box::new(tablet_end), "usb").map(|_| ()));
    let session = pc.connect_stream(Box::new(pc_end), "usb").unwrap();
    let control = session.open("echo", "c", Class::Control).unwrap();
    let mut buf = vec![0u8; 64];
    control.send(b"cable").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(5));
    // The PC's end of the cable goes; the tablet's stays (the relay keeps it open).
    frozen.store(true, std::sync::atomic::Ordering::SeqCst);
    relay_pc.shutdown(std::net::Shutdown::Both).unwrap();
    until("the PC's path lost", || session.path().is_none());
    assert_eq!(tablet.sessions()[0].path().as_deref(), Some("usb"), "the tablet still believes in it");
    let again = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert!(Arc::ptr_eq(&again, &session));
    until("the PC on the network", || session.path().as_deref() == Some("lan"));
    until("the tablet on the network", || tablet.sessions()[0].path().as_deref() == Some("lan"));
    control.send(b"still").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(5));
    drop(relay_tablet);
}
