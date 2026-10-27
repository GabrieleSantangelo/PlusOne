//! Datagrams: what must not wait (frames, input, sound) as UDP packets,
//! sealed with AES-256-GCM under a key fresh for each session, which goes to
//! the app over the link with the session's request (datagrams.py,
//! Datagrams.kt).
//!
//! A packet: nonce (12 bytes: 4 for the direction, FROM_PC or FROM_APP, 8
//! counting, never the same twice) | sealed body | tag. Only the app's
//! address is listened to, and only its packets newer than the newest heard
//! (the app sends input as states: an older one must not undo a newer).
//!
//! Many packets go in one call (send_all): UDP GSO, the kernel cuts them
//! (a keyframe of 400 KB in 0.3 ms instead of 2.3 in Python, measured).

use openssl::cipher::Cipher;
use openssl::cipher_ctx::CipherCtx;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const FROM_PC: u32 = 1;
pub const FROM_APP: u32 = 2;
pub const NONCE: usize = 12;
pub const TAG: usize = 16;
pub const KEY_BYTES: usize = 32;
pub const MTU_PAYLOAD: usize = 1472;  // what a packet may carry, all of it: nonce, body, tag
const GSO_MAX: usize = 44;            // packets of MTU_PAYLOAD in one call: under 64 KiB
const UDP_SEGMENT: libc::c_int = 103;

struct Sealer {
    ctx: CipherCtx,
    count: u64,
    batch: Vec<u8>,
}

pub struct Datagrams {
    pub key: [u8; KEY_BYTES],
    sock: UdpSocket,
    app: Mutex<Option<SocketAddr>>,
    sealer: Mutex<Sealer>,
    opener: Mutex<CipherCtx>,
    heard: AtomicU64,  // the app's highest count + 1 (0: none yet)
    gso: bool,
}

impl Datagrams {
    pub fn new(key: Option<[u8; KEY_BYTES]>) -> io::Result<Datagrams> {
        let key = match key {
            Some(key) => key,
            None => {
                let mut key = [0u8; KEY_BYTES];
                openssl::rand::rand_bytes(&mut key).map_err(io::Error::other)?;
                key
            }
        };
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        // Room for a keyframe's packets at once (as much as net.core.wmem_max allows).
        set_int(&sock, libc::SOL_SOCKET, libc::SO_SNDBUF, 4 << 20);
        let ctx = |encrypt: bool| -> io::Result<CipherCtx> {
            let mut ctx = CipherCtx::new().map_err(io::Error::other)?;
            if encrypt {
                ctx.encrypt_init(Some(Cipher::aes_256_gcm()), Some(&key), None).map_err(io::Error::other)?;
            } else {
                ctx.decrypt_init(Some(Cipher::aes_256_gcm()), Some(&key), None).map_err(io::Error::other)?;
            }
            Ok(ctx)
        };
        Ok(Datagrams {
            key,
            sock,
            app: Mutex::new(None),
            sealer: Mutex::new(Sealer { ctx: ctx(true)?, count: 0, batch: Vec::new() }),
            opener: Mutex::new(ctx(false)?),
            heard: AtomicU64::new(0),
            gso: true,
        })
    }

    pub fn port(&self) -> u16 {
        self.sock.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn app(&self) -> Option<SocketAddr> {
        *self.app.lock().unwrap()
    }

    pub fn set_app(&self, app: SocketAddr) {
        *self.app.lock().unwrap() = Some(app);
    }

    /// body to the app, sealed (nothing until set_app).
    pub fn send(&self, body: &[u8]) -> io::Result<()> {
        self.send_all(&[body])
    }

    /// bodies to the app, sealed, in order: as few calls as GSO allows
    /// (all of a call's packets but its last of one size).
    pub fn send_all(&self, bodies: &[&[u8]]) -> io::Result<()> {
        let Some(app) = *self.app.lock().unwrap() else { return Ok(()) };
        let mut sealer = self.sealer.lock().unwrap();
        let mut i = 0;
        while i < bodies.len() {
            // A run of same-sized packets (the last may be shorter), up to GSO_MAX.
            let size = bodies[i].len();
            let mut end = i + 1;
            while end < bodies.len() && end - i < GSO_MAX {
                let len = bodies[end].len();
                if len > size {
                    break;
                }
                end += 1;
                if len < size {
                    break;  // a shorter one ends the run
                }
            }
            let Sealer { ctx, count, batch } = &mut *sealer;
            batch.clear();
            for body in &bodies[i..end] {
                seal(ctx, *count, body, batch)?;
                *count += 1;
            }
            let segment = NONCE + size + TAG;
            if self.gso && end - i > 1 {
                if let Err(e) = send_gso(&self.sock, batch, segment, app) {
                    // No GSO here (an old kernel, a device without): one at a time.
                    if e.raw_os_error() != Some(libc::EIO) && e.raw_os_error() != Some(libc::EINVAL) {
                        return Err(e);
                    }
                    for chunk in batch.chunks(segment) {
                        self.sock.send_to(chunk, app)?;
                    }
                }
            } else {
                for chunk in batch.chunks(segment) {
                    self.sock.send_to(chunk, app)?;
                }
            }
            i = end;
        }
        Ok(())
    }

    /// Bytes sent and not yet on the air: in the socket, the queue, the
    /// Wi-Fi driver (each packet with its buffers, ~2.3 KB for 1472).
    pub fn queued(&self) -> usize {
        let mut n: libc::c_int = 0;
        let r = unsafe { libc::ioctl(self.sock.as_raw_fd(), libc::TIOCOUTQ, &mut n) };
        if r < 0 { 0 } else { n.max(0) as usize }
    }

    /// A body from the app, or None (nothing in time, or not the app's).
    pub fn receive(&self, timeout: Duration) -> Option<Vec<u8>> {
        let mut fd = libc::pollfd { fd: self.sock.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut fd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) } <= 0 {
            return None;
        }
        let mut packet = [0u8; 2048];
        let (n, from) = self.sock.recv_from(&mut packet).ok()?;
        let app = (*self.app.lock().unwrap())?;
        if from.ip() != app.ip() || n < NONCE + TAG + 1 {
            return None;
        }
        let direction = u32::from_be_bytes(packet[..4].try_into().unwrap());
        let count = u64::from_be_bytes(packet[4..12].try_into().unwrap());
        if direction != FROM_APP || count < self.heard.load(Ordering::Relaxed) {
            return None;
        }
        let body = open(&mut self.opener.lock().unwrap(), &packet[..n])?;
        self.heard.store(count + 1, Ordering::Relaxed);
        Some(body)
    }
}

fn seal(ctx: &mut CipherCtx, count: u64, body: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    let start = out.len();
    out.extend_from_slice(&FROM_PC.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    let nonce: [u8; NONCE] = out[start..start + NONCE].try_into().unwrap();
    ctx.encrypt_init(None, None, Some(&nonce)).map_err(io::Error::other)?;
    let at = out.len();
    out.resize(at + body.len() + TAG, 0);
    let n = ctx.cipher_update(body, Some(&mut out[at..at + body.len()])).map_err(io::Error::other)?;
    ctx.cipher_final(&mut out[at + n..at + body.len()]).map_err(io::Error::other)?;
    ctx.tag(&mut out[at + body.len()..]).map_err(io::Error::other)?;
    Ok(())
}

fn open(ctx: &mut CipherCtx, packet: &[u8]) -> Option<Vec<u8>> {
    let (nonce, rest) = packet.split_at(NONCE);
    let (sealed, tag) = rest.split_at(rest.len() - TAG);
    ctx.decrypt_init(None, None, Some(nonce)).ok()?;
    ctx.set_tag(tag).ok()?;
    let mut body = vec![0u8; sealed.len() + 16];
    let n = ctx.cipher_update(sealed, Some(&mut body)).ok()?;
    let f = ctx.cipher_final(&mut body[n..]).ok()?;  // the tag checked here
    body.truncate(n + f);
    Some(body)
}

fn send_gso(sock: &UdpSocket, data: &[u8], segment: usize, to: SocketAddr) -> io::Result<()> {
    let SocketAddr::V4(to) = to else {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    };
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: to.port().to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from(*to.ip()).to_be() },
        sin_zero: [0; 8],
    };
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut control = [0u64; 4];  // room for one cmsg with a u16, aligned
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_name = &address as *const _ as *mut libc::c_void;
    header.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    header.msg_controllen = unsafe { libc::CMSG_SPACE(2) } as usize;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_UDP;
        (*cmsg).cmsg_type = UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(2) as usize;
        std::ptr::copy_nonoverlapping((segment as u16).to_ne_bytes().as_ptr(), libc::CMSG_DATA(cmsg), 2);
        if libc::sendmsg(sock.as_raw_fd(), &header, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn set_int(sock: &impl AsRawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) {
    unsafe {
        libc::setsockopt(sock.as_raw_fd(), level, name, &value as *const _ as *const libc::c_void,
                         std::mem::size_of::<libc::c_int>() as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's side, as Datagrams.kt has it: opens FROM_PC, seals FROM_APP.
    fn app_seal(key: &[u8; 32], count: u64, body: &[u8]) -> Vec<u8> {
        let mut ctx = CipherCtx::new().unwrap();
        ctx.encrypt_init(Some(Cipher::aes_256_gcm()), Some(key), None).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&FROM_APP.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        let nonce: [u8; 12] = out[..12].try_into().unwrap();
        ctx.encrypt_init(None, None, Some(&nonce)).unwrap();
        let mut sealed = vec![0u8; body.len()];
        let n = ctx.cipher_update(body, Some(&mut sealed)).unwrap();
        ctx.cipher_final(&mut sealed[n..]).unwrap();
        let mut tag = [0u8; 16];
        ctx.tag(&mut tag).unwrap();
        out.extend_from_slice(&sealed);
        out.extend_from_slice(&tag);
        out
    }

    #[test]
    fn a_frame_s_packets_reach_the_app_in_order_and_sealed() {
        let pc = Datagrams::new(None).unwrap();
        let app = UdpSocket::bind("127.0.0.1:0").unwrap();
        app.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        pc.set_app(app.local_addr().unwrap());
        let bodies: Vec<Vec<u8>> = (0..50).map(|i| vec![i as u8; if i == 49 { 300 } else { 1400 }]).collect();
        let refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
        pc.send_all(&refs).unwrap();
        let mut ctx = CipherCtx::new().unwrap();
        ctx.decrypt_init(Some(Cipher::aes_256_gcm()), Some(&pc.key), None).unwrap();
        for (i, body) in bodies.iter().enumerate() {
            let mut buf = [0u8; 2048];
            let n = app.recv(&mut buf).unwrap();
            assert_eq!(u32::from_be_bytes(buf[..4].try_into().unwrap()), FROM_PC);
            assert_eq!(u64::from_be_bytes(buf[4..12].try_into().unwrap()), i as u64);
            assert_eq!(&open(&mut ctx, &buf[..n]).unwrap(), body);
        }
    }

    #[test]
    fn only_the_app_s_newer_packets_come_through() {
        let pc = Datagrams::new(None).unwrap();
        let app = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = SocketAddr::from(([127, 0, 0, 1], pc.port()));
        pc.set_app(app.local_addr().unwrap());
        let t = Duration::from_millis(500);
        app.send_to(&app_seal(&pc.key, 5, b"five"), to).unwrap();
        assert_eq!(pc.receive(t).as_deref(), Some(&b"five"[..]));
        app.send_to(&app_seal(&pc.key, 4, b"four"), to).unwrap();  // older: a replay, or undoing a newer state
        assert_eq!(pc.receive(t), None);
        app.send_to(&app_seal(&[9; 32], 6, b"six"), to).unwrap();  // another key
        assert_eq!(pc.receive(t), None);
        app.send_to(&app_seal(&pc.key, 7, b"seven"), to).unwrap();
        assert_eq!(pc.receive(t).as_deref(), Some(&b"seven"[..]));
    }
}
