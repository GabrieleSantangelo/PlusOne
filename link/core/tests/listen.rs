//! A listener out of descriptors: alone in a process of its own, since it
//! takes them all.

use plusone_link::{Identity, Node};
use std::sync::Arc;
use std::time::Duration;

/// The CPU time this process took so far.
fn cpu() -> Duration {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let time = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    time(usage.ru_utime) + time(usage.ru_stime)
}

#[test]
fn a_listener_out_of_descriptors_waits_not_spins() {
    let made = rcgen::generate_simple_self_signed(vec!["plus-one".into()]).unwrap();
    let identity = Identity::new(made.cert.der().to_vec(), made.key_pair.serialize_der()).unwrap();
    let node = Node::new("tablet", &identity, Arc::new(|_: &str| false)).unwrap();
    let address = node.listen("127.0.0.1:0".parse().unwrap()).unwrap();
    // Every descriptor taken but one, the connection's: none left to accept it.
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    let was = limit.rlim_cur;
    limit.rlim_cur = (std::fs::read_dir("/proc/self/fd").unwrap().count() + 16) as libc::rlim_t;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let mut taken = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        taken.push(file);
    }
    taken.pop();
    let connection = std::net::TcpStream::connect(address).unwrap();
    let before = cpu();
    std::thread::sleep(Duration::from_secs(1));
    let spent = cpu() - before;
    drop(taken);
    limit.rlim_cur = was;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    assert!(spent < Duration::from_millis(300), "{spent:?} of CPU in a second");
    drop(connection);
    node.close();
}
