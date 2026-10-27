//! The few system calls std has no safe way to: message sockets, poll,
//! eventfd, sending without waiting.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

pub fn set_int(fd: RawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> bool {
    unsafe {
        libc::setsockopt(fd, level, name, &value as *const libc::c_int as *const libc::c_void,
                         std::mem::size_of::<libc::c_int>() as libc::socklen_t) == 0
    }
}

/// Two connected message sockets (one write, one message), each with room
/// for this many bytes queued.
pub fn seqpacket_pair(buffer: usize) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) })?;
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&pair.0, &pair.1] {
        set_int(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, buffer as libc::c_int);
        set_int(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, buffer as libc::c_int);
    }
    Ok(pair)
}

/// One message; with wait false, WouldBlock when there is no room.
pub fn send(fd: RawFd, data: &[u8], wait: bool) -> io::Result<usize> {
    let flags = libc::MSG_NOSIGNAL | if wait { 0 } else { libc::MSG_DONTWAIT };
    loop {
        let sent = unsafe { libc::send(fd, data.as_ptr() as *const libc::c_void, data.len(), flags) };
        if sent >= 0 {
            return Ok(sent as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// One datagram to an address, never waiting (WouldBlock: no room).
pub fn send_to(fd: RawFd, data: &[u8], to: &std::net::SocketAddr) -> io::Result<usize> {
    let (address, length) = sockaddr(to);
    let sent = unsafe {
        libc::sendto(fd, data.as_ptr() as *const libc::c_void, data.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                     &address as *const libc::sockaddr_storage as *const libc::sockaddr, length)
    };
    if sent < 0 { Err(io::Error::last_os_error()) } else { Ok(sent as usize) }
}

fn sockaddr(address: &std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let length = match address {
        std::net::SocketAddr::V4(a) => {
            let sin = unsafe { &mut *(&mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) };
            std::mem::size_of::<libc::sockaddr_in>()
        }
        std::net::SocketAddr::V6(a) => {
            let sin6 = unsafe { &mut *(&mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr = libc::in6_addr { s6_addr: a.ip().octets() };
            sin6.sin6_flowinfo = a.flowinfo();
            sin6.sin6_scope_id = a.scope_id();
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, length as libc::socklen_t)
}

/// One message, waiting for it: its whole length (more than buf holds: it
/// was cut), 0 at the end (the other end closed).
pub fn recv(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let got = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_TRUNC) };
        if got >= 0 {
            return Ok(got as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Whether fd has room for a message within timeout.
pub fn writable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut fds = [libc::pollfd { fd, events: libc::POLLOUT, revents: 0 }];
    Ok(poll(&mut fds, Some(timeout))? > 0)
}

/// Whether fd has something to read (or its end) within timeout (None:
/// as long as it takes).
pub fn readable(fd: RawFd, timeout: Option<Duration>) -> io::Result<bool> {
    let mut fds = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }];
    Ok(poll(&mut fds, timeout)? > 0)
}

pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    let ms = timeout.map(|t| t.as_millis().min(i32::MAX as u128) as libc::c_int).unwrap_or(-1);
    loop {
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if ready >= 0 {
            return Ok(ready as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Both directions of a socket ended: whoever waits on it wakes to its end.
pub fn shutdown(fd: RawFd) {
    unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
}

/// A thread waiting in poll() woken by another (an eventfd).
pub struct Wake(OwnedFd);

impl Wake {
    pub fn new() -> io::Result<Wake> {
        let fd = check(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
        Ok(Wake(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn wake(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.0.as_raw_fd(), &one as *const u64 as *const libc::c_void, 8) };
    }

    pub fn clear(&self) {
        let mut count: u64 = 0;
        unsafe { libc::read(self.0.as_raw_fd(), &mut count as *mut u64 as *mut libc::c_void, 8) };
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
