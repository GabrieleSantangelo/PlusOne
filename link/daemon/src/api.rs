//! The socket apps reach plus-one-link through: SOCK_SEQPACKET (one
//! message, one JSON object), only this user's processes. A reply that
//! opens a channel carries its descriptor (SCM_RIGHTS).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the socket's path is too long"));
    }
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let length = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((address, length as libc::socklen_t))
}

/// The listening socket at path (a stale one from before replaced), or the
/// one systemd passed (socket activation).
pub fn listen(path: &Path) -> io::Result<OwnedFd> {
    if std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id())
        && std::env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        return Ok(unsafe { OwnedFd::from_raw_fd(3) });
    }
    // Its folder ours alone, if we make it (one that is there, say the
    // home folder for --socket ~/link, is left as it is).
    if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
        std::fs::create_dir_all(parent)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(path);
    let fd = check(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) })?;
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let (address, length) = address(path)?;
    check(unsafe { libc::bind(fd, &address as *const libc::sockaddr_un as *const libc::sockaddr, length) })?;
    check(unsafe { libc::listen(fd, 16) })?;
    Ok(socket)
}

/// The next app, if it is this user's (anyone else's is shut out).
pub fn accept(listener: &OwnedFd) -> io::Result<Option<OwnedFd>> {
    let fd = check(unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) })?;
    let client = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    check(unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut credentials as *mut libc::ucred as *mut libc::c_void, &mut length)
    })?;
    Ok((credentials.uid == unsafe { libc::getuid() }).then_some(client))
}

/// One message from an app, and the descriptor it carries if any (a
/// stream for a path: "stream"; any other closed): None at its end; why
/// for one refused (too big, its descriptors cut short), the connection
/// as it was.
pub fn receive(client: RawFd, buf: &mut [u8]) -> io::Result<Option<Result<(usize, Option<OwnedFd>), &'static str>>> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    header.msg_controllen = space as _;
    let length = loop {
        let got = unsafe { libc::recvmsg(client, &mut header, libc::MSG_CMSG_CLOEXEC) };
        if got >= 0 {
            break got as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let mut fd = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&header);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    // Each one ours now: the first kept, any other closed.
                    let given = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i)));
                    fd.get_or_insert(given);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&header, cmsg);
        }
    }
    if length == 0 {
        return Ok(None);
    }
    if header.msg_flags & libc::MSG_TRUNC != 0 {
        return Ok(Some(Err("a request too big")));
    }
    if header.msg_flags & libc::MSG_CTRUNC != 0 {
        return Ok(Some(Err("too many descriptors")));
    }
    Ok(Some(Ok((length, fd))))
}

/// One message to an app, never waiting for room (an event: an app that
/// does not read loses it).
pub fn send_now(client: RawFd, message: &serde_json::Value) -> io::Result<()> {
    let bytes = message.to_string().into_bytes();
    let sent = unsafe { libc::send(client, bytes.as_ptr() as *const libc::c_void, bytes.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    if sent < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The app's connection shut, both ways: it sees its end (and so does
/// whoever reads it here).
pub fn hang_up(client: RawFd) {
    unsafe { libc::shutdown(client, libc::SHUT_RDWR) };
}

/// One message to an app, with a descriptor if there is one (ours closes
/// once sent: the app has its own).
pub fn send(client: RawFd, message: &serde_json::Value, fd: Option<OwnedFd>) -> io::Result<()> {
    send_message(client, &message.to_string().into_bytes(), fd.as_ref(), 0)
}

/// As send(), waiting for room within that long at most: an app that
/// takes nothing meanwhile fails it (TimedOut), and the descriptor is
/// closed (it never had it).
pub fn send_within(client: RawFd, message: &serde_json::Value, fd: Option<OwnedFd>, within: Duration) -> io::Result<()> {
    let bytes = message.to_string().into_bytes();
    let until = Instant::now() + within;
    loop {
        match send_message(client, &bytes, fd.as_ref(), libc::MSG_DONTWAIT) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let left = until.saturating_duration_since(Instant::now());
                let mut fds = [libc::pollfd { fd: client, events: libc::POLLOUT, revents: 0 }];
                let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
                if left.is_zero() || (unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) } == 0) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "the app takes nothing"));
                }
            }
            done => return done,
        }
    }
}

fn send_message(client: RawFd, bytes: &[u8], fd: Option<&OwnedFd>, flags: libc::c_int) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    if let Some(fd) = fd {
        header.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        header.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd.as_raw_fd());
        }
    }
    loop {
        let sent = unsafe { libc::sendmsg(client, &header, libc::MSG_NOSIGNAL | flags) };
        if sent >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A request with descriptors (as many as an app likes).
#[cfg(test)]
pub fn send_fds(client: RawFd, request: &[u8], fds: &[RawFd]) {
    let mut iov = libc::iovec { iov_base: request.as_ptr() as *mut libc::c_void, iov_len: request.len() };
    let size = std::mem::size_of_val(fds) as u32;
    let space = unsafe { libc::CMSG_SPACE(size) } as usize;
    let mut control = vec![0u8; space];
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    header.msg_controllen = space as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg) as *mut RawFd, fds.len());
        assert_eq!(libc::sendmsg(client, &header, 0), request.len() as isize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe: (its reading end, its writing end).
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut ends = [0; 2];
        assert_eq!(unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) }, 0);
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
    }

    /// Whether every writing end of the pipe is closed (reading it, its end).
    fn closed(reading: &OwnedFd) -> bool {
        let mut byte = 0u8;
        unsafe { libc::read(reading.as_raw_fd(), &mut byte as *mut u8 as *mut libc::c_void, 1) == 0 }
    }

    fn connection() -> (OwnedFd, OwnedFd) {
        let mut pair = [0; 2];
        assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, pair.as_mut_ptr()) }, 0);
        unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) }
    }

    #[test]
    fn a_second_descriptor_is_closed() {
        let (app, daemon) = connection();
        let (first, _) = pipe();
        let (reading, writing) = pipe();
        send_fds(app.as_raw_fd(), br#"{"op": "stream", "kind": "bt"}"#, &[first.as_raw_fd(), writing.as_raw_fd()]);
        drop(writing);
        let mut buf = vec![0u8; 1024];
        let (length, given) = receive(daemon.as_raw_fd(), &mut buf).unwrap().unwrap().unwrap();
        assert_eq!(&buf[..length], br#"{"op": "stream", "kind": "bt"}"#);
        assert!(given.is_some());
        assert!(closed(&reading));
    }

    #[test]
    fn descriptors_cut_short_refused_and_closed() {
        let (app, daemon) = connection();
        let (reading, writing) = pipe();
        send_fds(app.as_raw_fd(), br#"{"op": "devices"}"#, &[writing.as_raw_fd(); 8]);
        drop(writing);
        let mut buf = vec![0u8; 1024];
        assert_eq!(receive(daemon.as_raw_fd(), &mut buf).unwrap().unwrap().err(), Some("too many descriptors"));
        assert!(closed(&reading));
    }
}
