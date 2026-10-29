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
fn a_big_transfer_never_runs_ahead_of_the_stream() {
    let (pc, tablet, address) = pair();
    let (got, total) = mpsc::channel();
    tablet.serve("slow", 1, move |incoming| {
        let mut buf = vec![0u8; 1 << 20];
        let mut bytes = 0;
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
            bytes += n;
            std::thread::sleep(Duration::from_millis(5));  // a slow reader: the stream backs up
        }
        let _ = got.send(bytes);
    });
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let channel = session.open("slow", "control", Class::Control).unwrap();
    let chunk = vec![7u8; 64 << 10];
    let mut most = 0;
    for _ in 0..64 {
        channel.send(&chunk).unwrap();
        most = most.max(session.queued());
    }
    drop(channel);
    assert_eq!(total.recv_timeout(Duration::from_secs(10)).unwrap(), 64 * chunk.len());
    // Never more queued than a realtime packet may wait behind on a stream.
    assert!(most <= 256 << 10, "{most} bytes queued");
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
fn a_transfer_one_way_on_a_stream_goes_as_fast_as_the_stream() {
    // The cable: a stream whose buffers fill at once. What is queued while
    // it is full goes as soon as there is room, not at the next ping.
    let (pc, tablet, _address) = pair();
    let (got, total) = mpsc::channel();
    tablet.serve("sink", 1, move |incoming| {
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
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    for end in [&a, &b] {
        use std::os::unix::io::AsRawFd;
        plusone_link::sys::set_int(end.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, 16 << 10);
        plusone_link::sys::set_int(end.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, 16 << 10);
    }
    let accepter = tablet.clone();
    std::thread::spawn(move || accepter.accept_stream(Box::new(b), "usb").map(|_| ()));
    let session = pc.connect_stream(Box::new(a), "usb").unwrap();
    let channel = session.open("sink", "control", Class::Control).unwrap();
    let chunk = vec![7u8; 64 << 10];
    let started = Instant::now();
    for _ in 0..128 {
        channel.send(&chunk).unwrap();
    }
    drop(channel);
    assert_eq!(total.recv_timeout(Duration::from_secs(30)).unwrap(), 128 * chunk.len());
    assert!(started.elapsed() < Duration::from_secs(3), "8 MiB in {:?}", started.elapsed());
}

#[test]
fn realtime_goes_ahead_of_a_big_transfer_on_a_stream() {
    // A cable of about 20 MB/s one way (the PC's writes), a big transfer
    // on it: realtime and input go ahead of it, not behind its queue.
    let (pc, tablet, _address) = pair();
    tablet.serve("sink", 1, |incoming| {
        let mut buf = vec![0u8; 1 << 20];
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
        }
    });
    let (cable, far) = std::os::unix::net::UnixStream::pair().unwrap();
    cable.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let (mut reading, mut writing) = (cable.try_clone().unwrap(), cable);
    let pcs_end = plusone_link::stream::bridge(
        move |buf| {
            use std::io::Read;
            reading.read(buf).map_err(|e| if e.kind() == std::io::ErrorKind::WouldBlock {
                std::io::Error::from(std::io::ErrorKind::TimedOut)
            } else { e })
        },
        move |data| {
            use std::io::Write;
            std::thread::sleep(Duration::from_micros(data.len() as u64 / 20));  // 20 MB/s
            writing.write_all(data)
        },
        64 << 10).unwrap();
    let accepter = tablet.clone();
    std::thread::spawn(move || accepter.accept_stream(Box::new(far), "usb").map(|_| ()));
    let session = pc.connect_stream(Box::new(pcs_end), "usb").unwrap();
    let bulk = session.open("sink", "control", Class::Control).unwrap();
    let realtime = session.open("echo", "rt", Class::Realtime).unwrap();
    let sending = std::thread::spawn(move || {
        let chunk = vec![7u8; 64 << 10];
        for _ in 0..256 {
            if bulk.send(&chunk).is_err() {
                break;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(200));  // the transfer under way
    let mut rtts = Vec::new();
    let mut buf = vec![0u8; 2048];
    for i in 0..60u32 {
        let started = Instant::now();
        let mut packet = vec![0u8; 1000];
        packet[..4].copy_from_slice(&i.to_be_bytes());
        realtime.send(&packet).unwrap();
        while let Ok(Some(n)) = realtime.recv(&mut buf, Some(Duration::from_millis(500))) {
            if n >= 4 && buf[..4] == i.to_be_bytes() {
                rtts.push(started.elapsed());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    sending.join().unwrap();
    rtts.sort();
    assert!(rtts.len() >= 50, "{} of 60 came back", rtts.len());
    let p90 = rtts[rtts.len() * 9 / 10];
    assert!(p90 < Duration::from_millis(10), "realtime behind the transfer: p90 {p90:?}");
}

#[test]
fn a_slow_channel_is_neither_closed_nor_in_the_others_way() {
    // A feature that takes its time (two seconds before it reads): what is
    // sent to it waits (its window), the channel stays, and the session's
    // other channels go on meanwhile.
    let (pc, tablet, address) = pair();
    let (got, total) = mpsc::channel();
    tablet.serve("slow", 1, move |incoming| {
        std::thread::sleep(Duration::from_secs(2));
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
    let slow = session.open("slow", "control", Class::Control).unwrap();
    let sending = std::thread::spawn(move || {
        let chunk = vec![7u8; 64 << 10];
        for _ in 0..64 {
            slow.send(&chunk).unwrap();
        }
    });
    std::thread::sleep(Duration::from_millis(300));  // its window full by now
    let echo = session.open("echo", "c", Class::Control).unwrap();
    let mut buf = vec![0u8; 64];
    let started = Instant::now();
    echo.send(b"still here").unwrap();
    assert_eq!(echo.recv(&mut buf, WAIT).unwrap(), Some(10));
    assert!(started.elapsed() < Duration::from_millis(500), "the others waited {:?}", started.elapsed());
    sending.join().unwrap();
    assert_eq!(total.recv_timeout(Duration::from_secs(10)).unwrap(), 64 * (64 << 10));
}

#[test]
fn a_session_through_a_relay() {
    // Neither reaches the other: both dial a relay (here, on loopback), which
    // joins them and copies bytes; the session's TLS is end to end.
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
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = listener.local_addr().unwrap();
    std::thread::spawn(move || plusone_link::relay::serve(listener));
    let (tablet_side, pc_fingerprint) = (tablet.clone(), pc_fp.clone());
    std::thread::spawn(move || tablet_side.relay(relay, &pc_fingerprint, false, Duration::from_secs(2)).map(|_| ()));
    let session = pc.relay(relay, &tablet_fp, true, Duration::from_secs(2)).unwrap();
    assert_eq!(session.path().as_deref(), Some("relay"));
    assert_eq!(session.transport(), Some("tcp"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let mut buf = vec![0u8; 1 << 17];
    control.send(b"through the relay").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(17));
    // As fast as the relay copies: 32 MiB there and back on loopback.
    let started = Instant::now();
    let chunk = vec![7u8; 64 << 10];
    let sending = {
        let control = session.open("echo", "bulk", Class::Control).unwrap();
        std::thread::spawn(move || {
            let mut got = 0;
            let reader = std::thread::spawn({
                let control = control.try_clone().unwrap();
                move || {
                    let mut buf = vec![0u8; 1 << 17];
                    let mut got = 0;
                    while got < 512 * (64 << 10) {
                        match control.recv(&mut buf, WAIT) {
                            Ok(Some(n)) if n > 0 => got += n,
                            _ => break,
                        }
                    }
                    got
                }
            });
            for _ in 0..512 {
                control.send(&chunk).unwrap();
            }
            got += reader.join().unwrap();
            got
        })
    };
    let got = sending.join().unwrap();
    let elapsed = started.elapsed();
    assert_eq!(got, 512 * (64 << 10));
    println!("relay on loopback: {:.0} MB/s each way", got as f64 / elapsed.as_secs_f64() / 1e6);
}

#[test]
fn a_wifi_direct_path_is_one_on_both_sides() {
    // Taken as any TCP connection, it is what the PC says it is: the tablet
    // takes it over its LAN path, as the PC does (it closed it there).
    let (pc_id, pc_fp) = identity();
    let (tablet_id, tablet_fp) = identity();
    let tablet = Node::new("tablet", &tablet_id, trusting(&pc_fp)).unwrap();
    let address = tablet.listen("127.0.0.1:0".parse().unwrap()).unwrap();
    let pc = Node::new("pc", &pc_id, trusting(&tablet_fp)).unwrap();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    until("the tablet's session", || tablet.session(&pc_fp).is_some());
    let moved = pc.connect_as(address, Duration::from_secs(2), "p2p").unwrap();
    assert_eq!(moved.id, session.id);
    until("p2p on the PC", || session.path().as_deref() == Some("p2p"));
    let theirs = tablet.session(&pc_fp).unwrap();
    until("p2p on the tablet", || theirs.path().as_deref() == Some("p2p"));
}

#[test]
fn realtime_on_a_slow_stream_is_dropped_not_queued_for_seconds() {
    // Bluetooth's pace (about 100 KB/s) and realtime asked at twice that:
    // what goes back comes in a fraction of a second, the rest is dropped.
    let (pc, tablet, _address) = pair();
    let (cable, far) = std::os::unix::net::UnixStream::pair().unwrap();
    cable.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let (mut reading, mut writing) = (cable.try_clone().unwrap(), cable);
    let pcs_end = plusone_link::stream::bridge(
        move |buf| {
            use std::io::Read;
            reading.read(buf).map_err(|e| if e.kind() == std::io::ErrorKind::WouldBlock {
                std::io::Error::from(std::io::ErrorKind::TimedOut)
            } else { e })
        },
        move |data| {
            use std::io::Write;
            std::thread::sleep(Duration::from_micros(data.len() as u64 * 10));  // 100 KB/s
            writing.write_all(data)
        },
        4096).unwrap();
    let accepter = tablet.clone();
    std::thread::spawn(move || accepter.accept_stream(Box::new(far), "bt").map(|_| ()));
    let session = pc.connect_stream(Box::new(pcs_end), "bt").unwrap();
    let realtime = session.open("echo", "rt", Class::Realtime).unwrap();
    let receiving = {
        let realtime = realtime.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let mut waits = Vec::new();
            let until = Instant::now() + Duration::from_secs(4);
            while Instant::now() < until {
                if let Ok(Some(n)) = realtime.recv(&mut buf, Some(Duration::from_millis(200))) {
                    if n >= 16 {
                        let sent = u128::from_be_bytes(buf[..16].try_into().unwrap());
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
                        waits.push(Duration::from_micros((now - sent) as u64));
                    }
                }
            }
            waits
        })
    };
    for _ in 0..600 {  // 3 s at 200 packets a second of 1 KB
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
        let mut packet = vec![0u8; 1000];
        packet[..16].copy_from_slice(&now.to_be_bytes());
        realtime.send(&packet).unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut waits = receiving.join().unwrap();
    waits.sort();
    assert!(!waits.is_empty());
    let p50 = waits[waits.len() / 2];
    assert!(p50 < Duration::from_millis(600), "realtime waited {p50:?} ({} back)", waits.len());
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
    assert_eq!(session.transport(), Some("stream"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let video = session.open("echo", "v", Class::Realtime).unwrap();
    let mut buf = vec![0u8; 4096];
    control.send(b"by cable").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(8));

    // The cable pulled: the session waits; what is said meanwhile waits too.
    cable.shutdown(std::net::Shutdown::Both).unwrap();
    until("the path lost", || session.path().is_none());
    assert_eq!(session.transport(), None);
    control.send(b"while away").unwrap();
    let again = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert!(Arc::ptr_eq(&again, &session), "the same session, another path");
    until("on the network", || session.path().as_deref() == Some("lan"));
    assert_eq!(session.transport(), Some("tcp"));
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
        let mut packet = vec![0u8; plusone_link::session::MAX_DATAGRAM];
        packet[..4].copy_from_slice(&i.to_be_bytes());
        video.send(&packet).unwrap();
        if video.recv(&mut buf, Some(Duration::from_millis(200))).unwrap() == Some(plusone_link::session::MAX_DATAGRAM) {
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

/// The tablet takes QUIC too (its hello says where), so a TCP session moves
/// to a QUIC path to it; control, realtime and bulk go on there.
fn quic_pair() -> (Node, Node, SocketAddr) {
    let (pc, tablet, address) = pair();
    tablet.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    pc.upgrade_to_quic(true);
    (pc, tablet, address)
}

#[test]
fn a_tcp_session_moves_to_quic() {
    let (pc, _tablet, address) = quic_pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    while !session.by_quic() {
        assert!(started.elapsed() < Duration::from_secs(5), "never on QUIC");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.path().as_deref(), Some("lan"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let mut buf = vec![0u8; 1 << 17];
    control.send(b"by QUIC").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(7));
    // Realtime as QUIC datagrams, once a pong proved them.
    let realtime = session.open("echo", "video", Class::Realtime).unwrap();
    while session.rtt().is_none() {
        assert!(started.elapsed() < Duration::from_secs(5), "datagrams never proven");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut got = 0;
    for i in 0..500u32 {
        let mut packet = vec![0u8; plusone_link::session::MAX_DATAGRAM];
        packet[..4].copy_from_slice(&i.to_be_bytes());
        realtime.send(&packet).unwrap();
        if let Some(n) = realtime.recv(&mut buf, Some(Duration::from_millis(200))).unwrap() {
            assert_eq!(n, plusone_link::session::MAX_DATAGRAM);
            got += 1;
        }
    }
    assert!(got >= 495, "{got} of 500 came back");
    // What went has gone: nothing waits (the desktop leaves frames out on it).
    while session.queued() > 0 {
        assert!(started.elapsed() < Duration::from_secs(10), "{} bytes never left", session.queued());
        std::thread::sleep(Duration::from_millis(20));
    }
    // Bulk both ways: 32 MiB there and back.
    let bulk = session.open("echo", "bulk", Class::Control).unwrap();
    let reader = {
        let bulk = bulk.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 17];
            let mut got = 0usize;
            while got < 512 * (64 << 10) {
                match bulk.recv(&mut buf, WAIT) {
                    Ok(Some(n)) if n > 0 => got += n,
                    _ => break,
                }
            }
            got
        })
    };
    let begun = Instant::now();
    let chunk = vec![7u8; 64 << 10];
    for _ in 0..512 {
        bulk.send(&chunk).unwrap();
    }
    assert_eq!(reader.join().unwrap(), 512 * (64 << 10));
    eprintln!("32 MiB there and back by QUIC in {:.2} s", begun.elapsed().as_secs_f64());
    assert!(session.by_quic());
}

/// What each side's events said: (event, path, transport) as each came.
fn told(node: &Node) -> mpsc::Receiver<(&'static str, Option<String>, Option<&'static str>)> {
    let (tell, told) = mpsc::channel();
    node.on_event(move |event| {
        let said = match event {
            Event::Up(session) => ("up", session.path(), session.transport()),
            Event::Moved(session) => ("moved", session.path(), session.transport()),
            Event::Detached(session) => ("detached", session.path(), session.transport()),
            _ => return,
        };
        let _ = tell.send(said);
    });
    told
}

#[test]
fn a_move_from_tcp_to_quic_is_told_on_both_sides() {
    // The path's kind stays "lan": only its transport changes, and both
    // sides hear it (the tablet's label over the desktop says it).
    let (pc, tablet, address) = quic_pair();
    let (pc_told, tablet_told) = (told(&pc), told(&tablet));
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let lan = Some("lan".to_owned());
    for (side, told) in [("the PC", &pc_told), ("the tablet", &tablet_told)] {
        let next = || told.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| panic!("{side}: nothing told"));
        assert_eq!(next(), ("up", lan.clone(), Some("tcp")), "{side}");
        assert_eq!(next(), ("moved", lan.clone(), Some("quic")), "{side}");
    }
    assert!(session.by_quic());
    assert_eq!(session.transport(), Some("quic"));
    // Nothing more, past the old path's end at the latest (HANDOVER_CAP,
    // 3 s): no detach when it goes, nor a second move.
    std::thread::sleep(HANDOVER_CAP_AND_SOME);
    assert!(pc_told.try_recv().is_err() && tablet_told.try_recv().is_err());
}

/// session.rs's HANDOVER_CAP (3 s) and half a second.
const HANDOVER_CAP_AND_SOME: Duration = Duration::from_millis(3500);

#[test]
fn what_goes_across_a_move_to_quic_arrives_once_and_in_order() {
    // Numbered control messages there and back (echoed) while the TCP
    // session moves to QUIC (as the PC's upgrade does it, by hand here to
    // have them on the way): none lost, none twice, in order; the move told
    // once on each side, and nothing after it.
    let (pc, tablet, address) = pair();
    let quic = tablet.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    let (pc_told, tablet_told) = (told(&pc), told(&tablet));
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let control = session.open("echo", "numbered", Class::Control).unwrap();
    const COUNT: u32 = 2000;
    let sent = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let sender = {
        let (control, sent) = (control.try_clone().unwrap(), sent.clone());
        std::thread::spawn(move || {
            for i in 0..COUNT {
                control.send(&i.to_be_bytes()).unwrap();
                sent.store(i + 1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(500));
            }
        })
    };
    let echoed = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while got.len() < COUNT as usize {
            match control.recv(&mut buf, WAIT) {
                Ok(Some(4)) => got.push(u32::from_be_bytes(buf[..4].try_into().unwrap())),
                other => panic!("after {} back: {other:?}", got.len()),
            }
        }
        got
    });
    until("some sent by TCP", || sent.load(std::sync::atomic::Ordering::SeqCst) >= 200);
    let moving = pc.connect_quic(quic, Duration::from_secs(2), "lan").unwrap();
    assert!(Arc::ptr_eq(&moving, &session), "the same session, another path");
    let lan = Some("lan".to_owned());
    for (side, told) in [("the PC", &pc_told), ("the tablet", &tablet_told)] {
        let next = || told.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| panic!("{side}: nothing told"));
        assert_eq!(next(), ("up", lan.clone(), Some("tcp")), "{side}");
        assert_eq!(next(), ("moved", lan.clone(), Some("quic")), "{side}");
    }
    let at_move = sent.load(std::sync::atomic::Ordering::SeqCst);
    assert!(at_move < COUNT, "all sent before the move: send more");
    sender.join().unwrap();
    let got = echoed.join().unwrap();
    if let Some(i) = (0..COUNT as usize).find(|&i| got[i] != i as u32) {
        panic!("message {i} came back as {} ({at_move} sent at the move)", got[i]);
    }
    assert_eq!(session.transport(), Some("quic"));
    std::thread::sleep(HANDOVER_CAP_AND_SOME);
    assert!(pc_told.try_recv().is_err() && tablet_told.try_recv().is_err(), "told more after the move");
    assert!(!session.is_closed());
}

#[test]
fn a_session_by_quic_alone() {
    let (pc, tablet, _address) = pair();
    let (events, budgets) = mpsc::channel();
    pc.on_event(move |event| {
        if let Event::Budget(session, rate) = event {
            let _ = events.send((session.peer.clone(), rate));
        }
    });
    let quic = tablet.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    let session = pc.connect_quic(quic, Duration::from_secs(2), "lan").unwrap();
    assert!(session.by_quic());
    assert_eq!(session.transport(), Some("quic"));
    let control = session.open("echo", "c", Class::Control).unwrap();
    let mut buf = vec![0u8; 1 << 10];
    control.send(b"hello").unwrap();
    assert_eq!(control.recv(&mut buf, WAIT).unwrap(), Some(5));
    // Its controller's budget, told as it began (and as it moves).
    let (peer, rate) = budgets.recv_timeout(Duration::from_secs(5)).expect("no budget told");
    assert_eq!(peer, session.peer);
    assert!(rate > 0);
}

/// A stream at about 100 KB/s, as Bluetooth goes: the PC's end and the
/// session on it.
fn slow_session() -> (Node, Node, Arc<plusone_link::Session>) {
    let (pc, tablet, _address) = pair();
    let session = bluetooth(&pc, &tablet, 100_000);
    (pc, tablet, session)
}

/// A Bluetooth path from pc to tablet, a stream that takes bytes a second
/// in pieces of about RFCOMM's frames (what goes back as fast as it
/// comes): the session on it.
fn bluetooth(pc: &Node, tablet: &Node, bytes: u64) -> Arc<plusone_link::Session> {
    let (cable, far) = std::os::unix::net::UnixStream::pair().unwrap();
    cable.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let (mut reading, mut writing) = (cable.try_clone().unwrap(), cable);
    let pcs_end = plusone_link::stream::bridge(
        move |buf| {
            use std::io::Read;
            reading.read(buf).map_err(|e| if e.kind() == std::io::ErrorKind::WouldBlock {
                std::io::Error::from(std::io::ErrorKind::TimedOut)
            } else { e })
        },
        move |data| {
            use std::io::Write;
            std::thread::sleep(Duration::from_micros(data.len() as u64 * 1_000_000 / bytes));
            writing.write_all(data)
        },
        1024).unwrap();
    let accepter = tablet.clone();
    std::thread::spawn(move || accepter.accept_stream(Box::new(far), "bt").map(|_| ()));
    pc.connect_stream(Box::new(pcs_end), "bt").unwrap()
}

#[test]
fn a_bluetooth_paths_budget_is_what_its_stream_drains() {
    // Its prior (0.9 Mbit/s) told as it begins; a transfer filling it, what
    // it drains told within 2 s, within a quarter: 1 Mbit/s, and 0.4 (under
    // the prior: never more than the path drains).
    for rate in [125_000u64, 50_000] {
        let (pc, tablet, _address) = pair();
        tablet.serve("sink", 1, |incoming| {
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
                if n == 0 {
                    break;
                }
            }
        });
        let (tell, told) = mpsc::channel();
        pc.on_event(move |event| {
            if let Event::Budget(_, rate) = event {
                let _ = tell.send(rate);
            }
        });
        let session = bluetooth(&pc, &tablet, rate);
        // From the moment it is there (plus-one-link tells it with up).
        assert_eq!(session.rate(), Some(900_000 / 8));
        assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(900_000 / 8), "its prior, as it began");
        let sink = session.open("sink", "bulk", Class::Control).unwrap();
        let begun = Instant::now();
        let sending = std::thread::spawn(move || {
            let chunk = vec![7u8; 32 << 10];
            while begun.elapsed() < Duration::from_secs(3) && sink.send(&chunk).is_ok() {}
        });
        std::thread::sleep(Duration::from_secs(2));
        let path = session.paths().pop().expect("no path");
        let near = |bytes: u64| bytes * 4 >= rate * 3 && bytes * 4 <= rate * 5;
        let measured = path.budget_bps.unwrap_or(0) / 8;
        assert!(path.budget_from == "drain" && near(measured), "{rate} B/s: {path:?}");
        let last = told.try_iter().last().unwrap_or(900_000 / 8);
        assert!(near(last), "{rate} B/s: {last} B/s told last");
        sending.join().unwrap();
        pc.close();
        tablet.close();
    }
}

#[test]
fn a_channel_opens_ahead_of_a_transfer_on_a_slow_path() {
    // A transfer fills its channel's queue (128 KiB: more than a second at
    // this pace); a channel opened meanwhile, and a message there and back
    // on it, go ahead of it: the session's messages first, the channels in
    // turn. (They waited behind it, and an open went unanswered.)
    let (_pc, _tablet, session) = slow_session();
    let bulk = session.open("echo", "bulk", Class::Control).unwrap();
    let draining = {
        let bulk = bulk.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 17];
            while let Ok(Some(n)) = bulk.recv(&mut buf, Some(Duration::from_secs(3))) {
                if n == 0 {
                    break;
                }
            }
        })
    };
    let chunk = vec![7u8; 32 << 10];
    let sending = std::thread::spawn(move || {
        for _ in 0..40 {
            if bulk.send(&chunk).is_err() {
                break;
            }
        }
        bulk
    });
    std::thread::sleep(Duration::from_millis(500));  // the transfer's queue full
    let begun = Instant::now();
    let other = session.open("echo", "other", Class::Control).unwrap();
    other.send(b"ahead").unwrap();
    let mut buf = vec![0u8; 64];
    assert_eq!(other.recv(&mut buf, WAIT).unwrap(), Some(5));
    let took = begun.elapsed();
    // (3.3 s before. What is left: the transfer's whole messages already on
    // their way, 32 KiB each at 100 KB/s; a message is not cut short.)
    assert!(took < Duration::from_millis(1500), "an open and a round trip took {took:?} behind a transfer");
    drop(sending.join().unwrap());
    let _ = draining.join();
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

#[test]
fn an_open_given_up_while_away_is_closed_on_the_other_side_too() {
    // The path lost, an open goes unanswered and the PC gives it up; it
    // went once the path came back, and the tablet took it: the channel is
    // closed there too (it was left open, with no one on the PC's side, as
    // long as the session lasted).
    let (pc, tablet, address) = pair();
    let (told, heard) = mpsc::channel();
    tablet.serve("late", 1, move |incoming| {
        let _ = told.send("opened");
        let mut buf = [0u8; 64];
        while let Ok(Some(n)) = incoming.channel.recv(&mut buf, None) {
            if n == 0 {
                break;
            }
        }
        let _ = told.send("closed");
    });
    let (via, cut) = proxy(address);
    let session = pc.connect(via, Duration::from_secs(2)).unwrap();
    until("the tablet's session", || tablet.sessions().first().is_some_and(|s| s.path().is_some()));
    cut();
    until("the path lost", || session.path().is_none());
    assert!(session.open("late", "x", Class::Control).err().unwrap().contains("no answer"));
    let again = pc.connect(address, Duration::from_secs(2)).unwrap();
    assert!(Arc::ptr_eq(&again, &session), "the same session, another path");
    assert_eq!(heard.recv_timeout(Duration::from_secs(5)), Ok("opened"));
    assert_eq!(heard.recv_timeout(Duration::from_secs(5)), Ok("closed"), "the tablet's channel left open");
    until("no channel on the tablet", || tablet.sessions().first().is_some_and(|s| s.channels() == 0));
    assert_eq!(session.channels(), 0);
}

#[test]
fn a_host_that_trickles_its_bytes_is_given_up() {
    // Each byte in time for a read's own timeout (5 s), the handshake
    // never done: the PC gives up on it after 8 s (HANDSHAKE_WHOLE, as
    // the listening side does), where it waited for ever.
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().unwrap();
        let mut hello = [0u8; 4096];
        let _ = tcp.read(&mut hello);
        // A handshake record 4 KiB long, a byte a second.
        let mut trickle = vec![0x16, 0x03, 0x03, 0x10, 0x00];
        trickle.resize(5 + 4096, 0);
        for byte in trickle {
            if tcp.write_all(&[byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    let (pc_id, _) = identity();
    let pc = Node::new("pc", &pc_id, Arc::new(|_: &str| true)).unwrap();
    let started = Instant::now();
    let (done, dialed) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(pc.connect(address, Duration::from_secs(2)).map(|_| ()));
    });
    let result = dialed.recv_timeout(Duration::from_secs(12));
    assert!(result.as_ref().is_ok_and(|r| r.is_err()), "after 12 s: {result:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "given up after {:?}", started.elapsed());
}

#[test]
fn the_sixty_fifth_channel_is_refused() {
    let (pc, _tablet, address) = pair();
    let session = pc.connect(address, Duration::from_secs(2)).unwrap();
    let open: Vec<_> = (0..64).map(|i| session.open("echo", &format!("c{i}"), Class::Control).unwrap()).collect();
    let refused = session.open("echo", "one too many", Class::Control).err().unwrap();
    assert!(refused.contains("64 channels"), "{refused}");
    drop(open);
}
