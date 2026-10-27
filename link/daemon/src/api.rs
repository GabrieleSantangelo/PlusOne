//! The socket apps reach plus-one-link through: SOCK_SEQPACKET (one
//! message, one JSON object), only this user's processes. A reply that
//! opens a channel carries its descriptor (SCM_RIGHTS).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

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
    if let Some(parent) = path.parent() {
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
/// stream for a path: "stream"): None at its end.
pub fn receive(client: RawFd, buf: &mut [u8]) -> io::Result<Option<(usize, Option<OwnedFd>)>> {
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
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        if !cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            fd = Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd)));
        }
    }
    if length == 0 {
        return Ok(None);
    }
    if header.msg_flags & libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a request too big"));
    }
    Ok(Some((length, fd)))
}

/// One message to an app, never waiting for room (an event: an app that
/// does not read loses it).
pub fn send_now(client: RawFd, message: &serde_json::Value) -> io::Result<()> {
    let bytes = message.to_string().into_bytes();
    let sent = unsafe { libc::send(client, bytes.as_ptr() as *const libc::c_void, bytes.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    if sent < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// One message to an app, with a descriptor if there is one (ours closes
/// once sent: the app has its own).
pub fn send(client: RawFd, message: &serde_json::Value, fd: Option<OwnedFd>) -> io::Result<()> {
    let bytes = message.to_string().into_bytes();
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    if let Some(fd) = &fd {
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
        let sent = unsafe { libc::sendmsg(client, &header, libc::MSG_NOSIGNAL) };
        if sent >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
