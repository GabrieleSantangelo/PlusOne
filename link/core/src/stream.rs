//! What a session runs on: a byte stream (TCP; or a socket bridged to a
//! USB accessory or a Bluetooth socket), and whether there is IP under it,
//! for datagrams. Without IP every channel goes on the stream.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub trait Stream: Read + Write + AsRawFd + Send + Sync {
    fn set_nonblocking(&self, on: bool) -> io::Result<()>;
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// Both ways, at once: whoever waits on it wakes.
    fn shutdown(&self);
    fn try_clone(&self) -> io::Result<Box<dyn Stream>>;
    /// (the other side's, ours) when there is IP under it: datagrams then
    /// go as UDP between the two.
    fn addresses(&self) -> Option<(IpAddr, IpAddr)> {
        None
    }
    /// What it is, as Session::transport says: "tcp", or "stream" (a socket
    /// bridged to a cable, Bluetooth, a QUIC stream).
    fn transport(&self) -> &'static str {
        "stream"
    }
}

impl Stream for TcpStream {
    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        TcpStream::set_nonblocking(self, on)
    }

    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn shutdown(&self) {
        let _ = TcpStream::shutdown(self, Shutdown::Both);
    }

    fn try_clone(&self) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(TcpStream::try_clone(self)?))
    }

    fn addresses(&self) -> Option<(IpAddr, IpAddr)> {
        Some((self.peer_addr().ok()?.ip(), self.local_addr().ok()?.ip()))
    }

    fn transport(&self) -> &'static str {
        "tcp"
    }
}

impl Stream for UnixStream {
    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        UnixStream::set_nonblocking(self, on)
    }

    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn shutdown(&self) {
        let _ = UnixStream::shutdown(self, Shutdown::Both);
    }

    fn try_clone(&self) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(UnixStream::try_clone(self)?))
    }
}

/// A device that moves bytes (a USB accessory, a Bluetooth socket's
/// streams), bridged to a socket a session runs on: what read gives goes
/// in, what the session writes goes to write. Each on a thread of its own;
/// either failing (the cable pulled) ends both, and the session with them.
pub fn bridge(mut read: impl FnMut(&mut [u8]) -> io::Result<usize> + Send + 'static,
              mut write: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static, chunk: usize)
              -> io::Result<UnixStream> {
    let (ours, theirs) = UnixStream::pair()?;
    little_room(&theirs);
    let (mut inbound, mut outbound) = (ours.try_clone()?, ours);
    // Either side over: both are (the device's reads only time out, and a
    // socket pair's end cannot be seen from the side that only writes).
    let over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (over_in, over_out) = (over.clone(), over);
    std::thread::Builder::new().name("link-bridge-in".into()).spawn(move || {
        let mut buf = vec![0u8; chunk];
        loop {
            match read(&mut buf) {
                Ok(0) => continue,  // a zero-length packet
                Ok(n) => {
                    if inbound.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::Interrupted => {
                    // Nothing for a while: only the session gone ends this.
                    if over_in.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        over_in.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = inbound.shutdown(Shutdown::Both);
    })?;
    std::thread::Builder::new().name("link-bridge-out".into()).spawn(move || {
        // A byte short of a transfer: a full one is never a whole number of
        // packets, so it goes as one (usb_pieces would cut it in two).
        let mut buf = vec![0u8; chunk - 1];
        loop {
            match outbound.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        over_out.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = outbound.shutdown(Shutdown::Both);
    })?;
    Ok(theirs)
}

/// The system's room for what the session writes to a stream it does not
/// own the far end of (a bridge to the cable): no more than this waits in
/// the kernel, where nothing can go ahead of it (the session's urgent
/// frames go ahead in its own queue). The kernel doubles it.
pub const KERNEL_ROOM: libc::c_int = 8 << 10;

/// The session's end of a bridge: KERNEL_ROOM.
pub fn little_room(session_end: &impl std::os::fd::AsRawFd) {
    crate::sys::set_int(session_end.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, KERNEL_ROOM);
}

/// A TCP path: no Nagle, and no more than KERNEL_ROOM not yet sent waits
/// in the kernel (TCP_NOTSENT_LOWAT; what is sent and not yet
/// acknowledged is the network's, as it has to be).
pub fn tune_tcp(tcp: &std::net::TcpStream) {
    let _ = tcp.set_nodelay(true);
    const TCP_NOTSENT_LOWAT: libc::c_int = 25;  // linux/tcp.h (Android's too)
    crate::sys::set_int(std::os::fd::AsRawFd::as_raw_fd(tcp), libc::IPPROTO_TCP, TCP_NOTSENT_LOWAT, KERNEL_ROOM);
}

/// On a USB accessory, before the session: the app says it is there (this
/// and a number of its own, again every USB_AGAIN until answered), the PC
/// answers with USB_GO and the same number, then TLS. Each side drops what
/// comes before: bytes of an attempt before (the PC's, the app's, either
/// started first, or went and came back) are never taken for this one's.
pub const USB_READY: &[u8] = b"PLUSONE-USB-READY-1:";
pub const USB_GO: &[u8] = b"PLUSONE-USB-GO-1:";
pub const USB_AGAIN: std::time::Duration = std::time::Duration::from_secs(2);

/// The number in a READY or GO transfer, if it is one.
pub fn usb_number(prefix: &[u8], transfer: &[u8]) -> Option<[u8; 8]> {
    transfer.strip_prefix(prefix).filter(|rest| rest.len() == 8).map(|rest| rest.try_into().unwrap())
}

/// A USB bulk transfer a whole number of packets long does not end the
/// other side's read: such a one goes as two, the last a byte alone.
pub fn usb_pieces(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let cut = if !data.is_empty() && data.len() % 512 == 0 { data.len() - 1 } else { data.len() };
    [&data[..cut], &data[cut..]].into_iter().filter(|piece| !piece.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_piece_a_whole_number_of_packets() {
        for length in [1usize, 511, 512, 1024, 1500, 16384] {
            let data = vec![7u8; length];
            let pieces: Vec<&[u8]> = usb_pieces(&data).collect();
            assert_eq!(pieces.iter().map(|p| p.len()).sum::<usize>(), length);
            assert!(pieces.iter().all(|p| p.len() % 512 != 0), "{length}: {:?}", pieces.iter().map(|p| p.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_bridge_carries_both_ways() {
        let (device, far) = UnixStream::pair().unwrap();
        let (mut reader, mut writer) = (device.try_clone().unwrap(), device);
        let stream = bridge(move |buf| reader.read(buf), move |data| writer.write_all(data), 4096).unwrap();
        let (mut stream_in, mut stream_out) = (stream.try_clone().unwrap(), stream);
        let (mut far_in, mut far_out) = (far.try_clone().unwrap(), far);
        far_out.write_all(b"from the device").unwrap();
        let mut buf = [0u8; 15];
        stream_in.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"from the device");
        stream_out.write_all(b"to the device").unwrap();
        let mut buf = [0u8; 13];
        far_in.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"to the device");
    }
}
