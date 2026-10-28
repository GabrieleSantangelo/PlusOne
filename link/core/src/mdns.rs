//! Finding devices with mDNS (DNS-SD), no daemon needed: a query on
//! port 5353 (shared with Avahi, if it runs), on every interface, and
//! what answers within a moment. Android's responder answers this way, not
//! to a query from another port (legacy unicast).

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;
const PTR: u16 = 12;
const TXT: u16 = 16;
const SRV: u16 = 33;
const A: u16 = 1;

/// A service found: its instance's name, where it listens, its TXT record.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub instance: String,
    pub address: SocketAddr,
    pub txt: HashMap<String, String>,
}

/// The IPv4 addresses of the interfaces up, with multicast, not loopback.
fn interfaces() -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut at = list;
    while !at.is_null() {
        let entry = unsafe { &*at };
        let flags = entry.ifa_flags as libc::c_int;
        if !entry.ifa_addr.is_null() && unsafe { (*entry.ifa_addr).sa_family } as libc::c_int == libc::AF_INET
            && flags & libc::IFF_UP != 0 && flags & libc::IFF_MULTICAST != 0 && flags & libc::IFF_LOOPBACK == 0 {
            let address = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
            found.push(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)));
        }
        at = entry.ifa_next;
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

fn socket() -> io::Result<UdpSocket> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    crate::sys::set_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1);
    crate::sys::set_int(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1);
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: PORT.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    if unsafe { libc::bind(fd, &address as *const libc::sockaddr_in as *const libc::sockaddr,
                           std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    crate::sys::set_int(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_TTL, 255);
    Ok(socket)
}

fn query(service: &str) -> Vec<u8> {
    let mut message = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in service.trim_end_matches('.').split('.') {
        message.push(label.len() as u8);
        message.extend_from_slice(label.as_bytes());
    }
    message.extend_from_slice(&[0, 0, PTR as u8, 0, 1]);
    message
}

/// Who serves service ("_plusone-link._tcp.local"), within wait; done as
/// soon as stop says so of one found.
pub fn browse(service: &str, wait: Duration, stop: impl Fn(&Found) -> bool) -> io::Result<Vec<Found>> {
    let socket = socket()?;
    let fd = socket.as_raw_fd();
    let interfaces = interfaces();
    for interface in &interfaces {
        let _ = socket.join_multicast_v4(&GROUP, interface);
    }
    let message = query(service);
    let ask = || {
        for interface in &interfaces {
            let address = libc::in_addr { s_addr: u32::from(*interface).to_be() };
            unsafe {
                libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &address as *const libc::in_addr as *const libc::c_void,
                                 std::mem::size_of::<libc::in_addr>() as libc::socklen_t);
            }
            let _ = socket.send_to(&message, SocketAddrV4::new(GROUP, PORT));
        }
    };
    let started = Instant::now();
    // Asked again a little later: a first query can go unheard.
    let mut again = [Duration::from_millis(250), Duration::from_millis(1000)].into_iter();
    let mut next = again.next();
    ask();
    let mut records = Records::default();
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 9000];
    while started.elapsed() < wait {
        if next.is_some_and(|at| started.elapsed() >= at) {
            ask();
            next = again.next();
        }
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let Ok((length, from)) = socket.recv_from(&mut buf) else { continue };
        if records.read(&buf[..length]).is_none() {
            continue;
        }
        for item in records.found(service, from) {
            if !found.iter().any(|f| f.instance == item.instance && f.address == item.address) {
                let done = stop(&item);
                found.push(item);
                if done {
                    return Ok(found);
                }
            }
        }
    }
    Ok(found)
}

/// The records heard so far, by name.
#[derive(Default)]
struct Records {
    ptr: HashMap<String, Vec<String>>,
    srv: HashMap<String, (String, u16)>,
    txt: HashMap<String, HashMap<String, String>>,
    a: HashMap<String, Ipv4Addr>,
}

impl Records {
    /// A response's records; None if it is not one, or is broken.
    fn read(&mut self, message: &[u8]) -> Option<()> {
        if message.len() < 12 || message[2] & 0x80 == 0 {
            return None;
        }
        let count = |i: usize| u16::from_be_bytes([message[i], message[i + 1]]) as usize;
        let (questions, records) = (count(4), count(6) + count(8) + count(10));
        let mut at = 12;
        for _ in 0..questions {
            at = name(message, at)?.1 + 4;
        }
        for _ in 0..records {
            let (owner, after) = name(message, at)?;
            let header = message.get(after..after + 10)?;
            let kind = u16::from_be_bytes([header[0], header[1]]);
            let length = u16::from_be_bytes([header[8], header[9]]) as usize;
            let data = after + 10;
            message.get(data..data + length)?;
            match kind {
                PTR => self.ptr.entry(owner.to_lowercase()).or_default().push(name(message, data)?.0),
                SRV if length >= 7 => {
                    let port = u16::from_be_bytes([message[data + 4], message[data + 5]]);
                    self.srv.insert(owner.to_lowercase(), (name(message, data + 6)?.0.to_lowercase(), port));
                }
                TXT => {
                    let mut entries = HashMap::new();
                    let mut i = data;
                    while i < data + length {
                        let size = message[i] as usize;
                        let text = String::from_utf8_lossy(message.get(i + 1..i + 1 + size)?).into_owned();
                        if let Some((key, value)) = text.split_once('=') {
                            entries.insert(key.to_owned(), value.to_owned());
                        }
                        i += 1 + size;
                    }
                    self.txt.insert(owner.to_lowercase(), entries);
                }
                A if length == 4 => {
                    self.a.insert(owner.to_lowercase(), Ipv4Addr::new(message[data], message[data + 1], message[data + 2], message[data + 3]));
                }
                _ => {}
            }
            at = data + length;
        }
        Some(())
    }

    /// The instances of service complete enough to reach (the host's
    /// address unknown: the responder's, from).
    fn found(&self, service: &str, from: SocketAddr) -> Vec<Found> {
        let service = service.trim_end_matches('.').to_lowercase();
        let mut found = Vec::new();
        for instance in self.ptr.get(&service).into_iter().flatten() {
            let key = instance.to_lowercase();
            let Some((host, port)) = self.srv.get(&key) else { continue };
            let ip = self.a.get(host).copied().or(match from {
                SocketAddr::V4(v4) => Some(*v4.ip()),
                SocketAddr::V6(_) => None,
            });
            if let Some(ip) = ip {
                found.push(Found { instance: instance.clone(), address: SocketAddr::new(ip.into(), *port),
                                   txt: self.txt.get(&key).cloned().unwrap_or_default() });
            }
        }
        found
    }
}

/// What one response tells of service (link/fuzz: a stranger's packet).
#[cfg(fuzzing)]
pub fn read_response(service: &str, message: &[u8]) -> Vec<Found> {
    let mut records = Records::default();
    let _ = records.read(message);
    records.found(service, SocketAddr::from(([192, 0, 2, 1], 5353)))
}

/// A name at at (compression followed): it, and where the record goes on.
fn name(message: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut after = None;
    for _ in 0..128 {
        let length = *message.get(at)? as usize;
        if length == 0 {
            return Some((labels.join("."), after.unwrap_or(at + 1)));
        }
        if length & 0xc0 == 0xc0 {
            let pointer = ((length & 0x3f) << 8) | *message.get(at + 1)? as usize;
            after.get_or_insert(at + 2);
            at = pointer;
            continue;
        }
        labels.push(String::from_utf8_lossy(message.get(at + 1..at + 1 + length)?).into_owned());
        at += 1 + length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(out: &mut Vec<u8>, name: &str) {
        for part in name.split('.') {
            out.push(part.len() as u8);
            out.extend_from_slice(part.as_bytes());
        }
        out.push(0);
    }

    #[test]
    fn a_response_read() {
        // PTR, then SRV, TXT and A in the additional section, names compressed.
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3];
        let service_at = m.len();
        label(&mut m, "_plusone-link._tcp.local");
        m.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 120]);
        let instance_at = m.len() + 2;
        let mut instance = vec![3];
        instance.extend_from_slice(b"Tab");
        instance.extend_from_slice(&[0xc0, service_at as u8]);
        m.extend_from_slice(&(instance.len() as u16).to_be_bytes());
        m.extend_from_slice(&instance);
        m.extend_from_slice(&[0xc0, instance_at as u8, 0, 33, 0x80, 1, 0, 0, 0, 120]);
        let mut srv = vec![0, 0, 0, 0, 0xa9, 0x43];
        let host_at = m.len() + 2 + srv.len();
        label(&mut srv, "Android_X.local");
        m.extend_from_slice(&(srv.len() as u16).to_be_bytes());
        m.extend_from_slice(&srv);
        m.extend_from_slice(&[0xc0, instance_at as u8, 0, 16, 0x80, 1, 0, 0, 0, 120]);
        let txt = b"\x05v=910\x13id=6a2665bcebe1984c";
        m.extend_from_slice(&(txt.len() as u16).to_be_bytes());
        m.extend_from_slice(txt);
        m.extend_from_slice(&[0xc0, host_at as u8, 0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 10, 0, 0, 7]);
        let mut records = Records::default();
        records.read(&m).unwrap();
        let found = records.found("_plusone-link._tcp.local", "10.9.9.9:5353".parse().unwrap());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].instance, "Tab._plusone-link._tcp.local");
        assert_eq!(found[0].address, "10.0.0.7:43331".parse().unwrap());
        assert_eq!(found[0].txt["id"], "6a2665bcebe1984c");
        assert_eq!(found[0].txt["v"], "910");
    }

    #[test]
    fn a_query_built() {
        let q = query("_plusone-link._tcp.local");
        assert_eq!(&q[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(name(&q, 12).unwrap(), ("_plusone-link._tcp.local".into(), q.len() - 4));
    }
}
