//! QUIC's UDP socket (quic.rs endpoint), with the IP header's DSCP ours:
//! AF41 while realtime goes on the endpoint, either way, as the raw UDP
//! path's datagrams (session.rs, REALTIME_TOS here). Wi-Fi puts them in
//! its video queue (WMM AC_VI, RFC 8325), and the PC's Intel Wi-Fi, seeing
//! video go, scans in short pieces (iwlmvm: over 100 VI or VO frames in
//! 10 s; 44 ms off the channel at a time, not 120).
//!
//! quinn-udp puts the traffic class byte on each packet itself, in a
//! control message that overrides the socket's IP_TOS, and with the ECN
//! bits alone: QUIC's packets all went DSCP 0. This socket sends by
//! itself: quinn-udp's sendmsg, its control message the DSCP and quinn's
//! ECN bits. Receiving, GRO, and whether GSO is there are quinn-udp's.
//!
//! The whole endpoint is marked while realtime goes, the session's bulk
//! too: quinn tells the socket nothing of what a packet carries (that
//! would take patches to quinn-proto, quinn and quinn-udp), and the bulk
//! goes on what the controller leaves the realtime anyway (quic.rs). The
//! marking goes on at the first realtime and off only after MARKED_FOR
//! without any: its switches are rare.

use crate::log;
use quinn::udp::{RecvMeta, Transmit, UdpSocketState};
use quinn::{AsyncUdpSocket, UdpPoller};
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::Interest;

/// AF41 (34): Wi-Fi's video queue (WMM AC_VI, RFC 8325).
pub(crate) const AF41: u8 = 34;
/// The traffic class byte of realtime's packets, QUIC's and the raw UDP
/// path's datagrams (session.rs): AF41, the ECN bits clear.
pub(crate) const REALTIME_TOS: u8 = AF41 << 2;
/// An endpoint's packets stay marked this long after realtime's last
/// datagram, either way; they are marked at once at the first. A marked
/// packet takes the video queue and may pass unmarked ones still in the
/// best effort queue: quinn takes 3 packets out of order for a loss, and
/// its controller cuts just as realtime starts. So the switch on must be
/// rare (not at each pen stroke, nor after each pause of the sound), and
/// only the switch on reorders. 10 s is iwlmvm's window too (over 100 VI
/// or VO frames in 10 s); session.rs's REALTIME_GOING (500 ms) is the
/// pace's, a matter of rates.
const MARKED_FOR: Duration = Duration::from_secs(10);
/// linux/udp.h's; the same on Android, whose libc crate lacks it.
const UDP_SEGMENT: libc::c_int = 103;
/// Room for the control messages: the traffic class, GSO's segment size
/// and the source address (in6_pktinfo, the biggest), 88 bytes at most.
const CONTROL: usize = 128;
/// A kind of send error (the packet lost) is logged once in this long at
/// most (quinn-udp's interval).
const LOG_EVERY: Duration = Duration::from_secs(60);

/// QUIC's endpoints on this socket (quic.rs endpoint): on unless
/// PLUSONE_LINK_DSCP=0 (a live A/B against the way before it, with no
/// build: quinn's own socket, every QUIC packet DSCP 0). Read once; the
/// raw UDP path's datagrams stay AF41 either way, as before.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("PLUSONE_LINK_DSCP").map_or(true, |v| v.trim() != "0");
        if !on {
            log!("QUIC unmarked (PLUSONE_LINK_DSCP=0): quinn's own socket, every packet DSCP 0, as before");
        }
        on
    })
}

/// Whether an endpoint's packets go as video now: realtime went within
/// MARKED_FOR. Atomics alone, and nothing runs while idle: each send reads
/// the time of realtime's last datagram.
#[derive(Debug)]
pub(crate) struct Marking {
    since: Instant,
    last_us: AtomicU64,  // since `since`: realtime's last datagram, either way (0: none yet)
}

impl Marking {
    pub(crate) fn new() -> Marking {
        Marking { since: Instant::now(), last_us: AtomicU64::new(0) }
    }

    /// Realtime went now, either way: the endpoint's packets AF41 until
    /// MARKED_FOR without any.
    pub(crate) fn realtime(&self) {
        self.realtime_at(self.now_us());
    }

    fn dscp(&self) -> u8 {
        self.dscp_at(self.now_us())
    }

    /// Now, since `since`: 1 at least (0 is none).
    fn now_us(&self) -> u64 {
        (self.since.elapsed().as_micros() as u64).max(1)
    }

    /// realtime at us (since `since`, as now_us): another thread's older
    /// time never moves it back.
    fn realtime_at(&self, us: u64) {
        self.last_us.fetch_max(us, Ordering::Relaxed);
    }

    /// The DSCP at us (since `since`, as now_us).
    fn dscp_at(&self, us: u64) -> u8 {
        let last = self.last_us.load(Ordering::Relaxed);
        if last != 0 && us.saturating_sub(last) < MARKED_FOR.as_micros() as u64 { AF41 } else { 0 }
    }
}

/// quinn's socket (its TokioRuntime's), sending with our traffic class.
#[derive(Debug)]
pub(crate) struct Socket {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
    marking: Arc<Marking>,
    /// A GSO send failed (a driver without it): one datagram at a time.
    no_gso: AtomicBool,
    /// The system refused the traffic class (EINVAL, and the packet went
    /// without it; quinn-udp's fallback): packets go without, unmarked.
    no_tos: AtomicBool,
    /// When each kind of send error (errno) was last logged.
    logged: Mutex<Vec<(i32, Instant)>>,
}

impl Socket {
    /// On the current runtime.
    pub(crate) fn new(socket: std::net::UdpSocket, marking: Arc<Marking>) -> io::Result<Socket> {
        let state = UdpSocketState::new((&socket).into())?;  // non-blocking too
        Ok(Socket {
            io: tokio::net::UdpSocket::from_std(socket)?,
            state,
            marking,
            no_gso: AtomicBool::new(false),
            no_tos: AtomicBool::new(false),
            logged: Mutex::new(Vec::new()),
        })
    }

    /// transmit out with tos as its traffic class byte: WouldBlock with no
    /// room; any other error is the packet lost (quinn-udp's are too: QUIC
    /// sends it again), and logged once a minute at most.
    fn send(&self, transmit: &Transmit, tos: u8) -> io::Result<()> {
        // As quinn-udp: one segment's worth at most is no GSO (drivers balk).
        let gso = transmit.segment_size.filter(|&size| size < transmit.contents.len());
        let mut tos = if self.no_tos.load(Ordering::Relaxed) { None } else { Some(tos) };
        let mut refused = None;  // the traffic class's error, while the packet goes again without
        loop {
            let error = match self.sendmsg(transmit, gso, tos) {
                Ok(()) => {
                    if let Some(error) = refused {
                        if !self.no_tos.swap(true, Ordering::Relaxed) {
                            log!("QUIC: the traffic class refused ({error}): packets go unmarked");
                        }
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Err(error),
                Err(error) => error,
            };
            match error.raw_os_error() {
                // A driver without GSO says so only this way (quinn-udp's
                // check): the batch lost, the next ones one at a time.
                Some(libc::EIO | libc::EINVAL) if gso.is_some() => {
                    if self.no_gso.swap(true, Ordering::Relaxed) {
                        self.log_error(&error, transmit);  // a batch quinn had made before
                    } else {
                        log!("QUIC: no segmentation offload ({error}): one datagram at a time");
                    }
                    return Ok(());
                }
                // The traffic class refused? Once more without it (as
                // quinn-udp), and never again if that goes: unmarked, not
                // every packet lost.
                Some(libc::EINVAL) if tos.is_some() => {
                    tos = None;
                    refused = Some(error);
                }
                // An MTU probe bigger than the path: expected (quinn-udp
                // says nothing either).
                Some(libc::EMSGSIZE) => return Ok(()),
                _ => {
                    self.log_error(&error, transmit);
                    return Ok(());
                }
            }
        }
    }

    /// One sendmsg of transmit: gso its segment size, tos its traffic class
    /// byte (None: the socket's, 0), the source address quinn names.
    fn sendmsg(&self, transmit: &Transmit, gso: Option<usize>, tos: Option<u8>) -> io::Result<()> {
        let (address, address_length) = crate::sys::sockaddr(&transmit.destination);
        let mut iov = libc::iovec { iov_base: transmit.contents.as_ptr() as *mut libc::c_void,
                                    iov_len: transmit.contents.len() };
        let mut control = Control([0; CONTROL]);
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = &address as *const libc::sockaddr_storage as *mut libc::c_void;
        msg.msg_namelen = address_length;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.0.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = CONTROL as _;
        let mut cmsgs = Cmsgs::new(&mut msg);
        if let Some(tos) = tos {
            // IPv4, or IPv4 mapped on a dual-stack socket: the IPv4 header's.
            let ipv4 = match transmit.destination.ip() {
                IpAddr::V4(_) => true,
                IpAddr::V6(ip) => ip.to_ipv4_mapped().is_some(),
            };
            if ipv4 {
                cmsgs.push(libc::IPPROTO_IP, libc::IP_TOS, tos as libc::c_int);
            } else {
                cmsgs.push(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, tos as libc::c_int);
            }
        }
        if let Some(size) = gso {
            cmsgs.push(libc::SOL_UDP, UDP_SEGMENT, size as u16);
        }
        match transmit.src_ip {
            Some(IpAddr::V4(ip)) => cmsgs.push(libc::IPPROTO_IP, libc::IP_PKTINFO, libc::in_pktinfo {
                ipi_ifindex: 0,
                ipi_spec_dst: libc::in_addr { s_addr: u32::from_ne_bytes(ip.octets()) },
                ipi_addr: libc::in_addr { s_addr: 0 },
            }),
            Some(IpAddr::V6(ip)) => cmsgs.push(libc::IPPROTO_IPV6, libc::IPV6_PKTINFO, libc::in6_pktinfo {
                ipi6_addr: libc::in6_addr { s6_addr: ip.octets() },
                ipi6_ifindex: 0,
            }),
            None => {}
        }
        cmsgs.finish();
        loop {
            if unsafe { libc::sendmsg(self.io.as_raw_fd(), &msg, 0) } >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// A packet lost to error: a line, unless one went for its kind within
    /// LOG_EVERY (a link down fails every packet).
    fn log_error(&self, error: &io::Error, transmit: &Transmit) {
        let kind = error.raw_os_error().unwrap_or(0);
        let now = Instant::now();
        {
            let mut logged = self.logged.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match logged.iter_mut().find(|(logged_kind, _)| *logged_kind == kind) {
                Some((_, at)) if now.duration_since(*at) < LOG_EVERY => return,
                Some((_, at)) => *at = now,
                None => logged.push((kind, now)),
            }
        }
        log!("QUIC: a packet to {} lost ({error}; said once a minute at most)", transmit.destination);
    }
}

impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Writable {
            wait: move || {
                let socket = self.clone();
                async move { socket.io.writable().await }
            },
            waiting: None,
        })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let tos = self.marking.dscp() << 2 | transmit.ecn.map_or(0, |ecn| ecn as u8);
        self.io.try_io(Interest::WRITABLE, || self.send(transmit, tos))
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta])
                 -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            if let Ok(got) = self.io.try_io(Interest::READABLE, || self.state.recv((&self.io).into(), bufs, meta)) {
                return Poll::Ready(Ok(got));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        if self.no_gso.load(Ordering::Relaxed) { 1 } else { self.state.max_gso_segments() }
    }

    fn max_receive_segments(&self) -> usize {
        self.state.gro_segments()
    }

    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
}

/// A task's wait for room in the socket, as quinn's own (its
/// UdpPollHelper): each poller its own, so any number of them wait at once
/// (tokio's poll_send_ready wakes the last alone), and held in place:
/// quinn asks before each packet, and nothing is allocated for it.
struct Writable<Wait, Waiting> {
    /// A new wait (an Arc's clone, no allocation).
    wait: Wait,
    waiting: Option<Waiting>,
}

impl<Wait, Waiting> std::fmt::Debug for Writable<Wait, Waiting> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Writable")
    }
}

impl<Wait, Waiting> UdpPoller for Writable<Wait, Waiting>
where
    Wait: Fn() -> Waiting + Send + Sync + 'static,
    Waiting: Future<Output = io::Result<()>> + Send + Sync + 'static,
{
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // SAFETY: waiting is pinned with self (structurally, as quinn's
        // pin_project does): it is only ever set from None or dropped in
        // place, never moved; wait is not pinned and only called.
        let this = unsafe { self.get_unchecked_mut() };
        let waiting = this.waiting.get_or_insert_with(&this.wait);
        let ready = unsafe { Pin::new_unchecked(waiting) }.poll(cx);
        if ready.is_ready() {
            this.waiting = None;  // a future polled once ready no more
        }
        ready
    }
}

#[repr(C, align(8))]
struct Control([u8; CONTROL]);

/// A message's control messages, written one after the other.
struct Cmsgs<'a> {
    msg: &'a mut libc::msghdr,
    next: *mut libc::cmsghdr,
    length: usize,
}

impl<'a> Cmsgs<'a> {
    /// msg's control: zeroes, CONTROL long.
    fn new(msg: &'a mut libc::msghdr) -> Cmsgs<'a> {
        let next = unsafe { libc::CMSG_FIRSTHDR(msg) };
        Cmsgs { msg, next, length: 0 }
    }

    fn push<T: Copy>(&mut self, level: libc::c_int, kind: libc::c_int, value: T) {
        let size = std::mem::size_of::<T>() as libc::c_uint;
        let space = unsafe { libc::CMSG_SPACE(size) } as usize;
        assert!(!self.next.is_null() && self.length + space <= CONTROL, "control messages past their room");
        unsafe {
            (*self.next).cmsg_level = level;
            (*self.next).cmsg_type = kind;
            (*self.next).cmsg_len = libc::CMSG_LEN(size) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(self.next) as *mut T, value);
            self.next = libc::CMSG_NXTHDR(self.msg, self.next);
        }
        self.length += space;
    }

    fn finish(self) {
        self.msg.msg_controllen = self.length as _;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn::udp::EcnCodepoint;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const SECOND: u64 = 1_000_000;

    #[test]
    fn marked_at_the_first_realtime_and_off_after_10_s_without() {
        let marking = Marking::new();
        assert_eq!(marking.dscp_at(5 * SECOND), 0, "before any realtime");
        marking.realtime_at(5 * SECOND);
        assert_eq!(marking.dscp_at(5 * SECOND), AF41, "at the first realtime, at once");
        // Pen strokes 2 s apart: marked all along, never switched.
        for stroke in [7, 9, 11] {
            assert_eq!(marking.dscp_at(stroke * SECOND - 1), AF41, "before the stroke at {stroke} s");
            marking.realtime_at(stroke * SECOND);
            assert_eq!(marking.dscp_at(stroke * SECOND + 1), AF41, "after the stroke at {stroke} s");
        }
        // 10 s with none: off.
        assert_eq!(marking.dscp_at(21 * SECOND - 1), AF41, "just under 10 s idle");
        assert_eq!(marking.dscp_at(21 * SECOND), 0, "10 s idle");
        assert_eq!(marking.dscp_at(60 * SECOND), 0, "idle since");
        // Realtime again: on at once; an older time told late (another
        // thread's) moves nothing back.
        marking.realtime_at(70 * SECOND);
        marking.realtime_at(61 * SECOND);
        assert_eq!(marking.dscp_at(79 * SECOND), AF41);
        assert_eq!(marking.dscp_at(80 * SECOND), 0);
    }

    /// A plain UDP socket told each datagram's traffic class byte.
    fn observer(address: SocketAddr) -> io::Result<std::net::UdpSocket> {
        let socket = std::net::UdpSocket::bind(address)?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        let told = if address.is_ipv4() {
            crate::sys::set_int(socket.as_raw_fd(), libc::IPPROTO_IP, libc::IP_RECVTOS, 1)
        } else {
            crate::sys::set_int(socket.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS, 1)
        };
        assert!(told);
        Ok(socket)
    }

    /// The next datagram's length and traffic class byte.
    fn received(socket: &std::net::UdpSocket) -> (usize, u8) {
        let mut buf = [0u8; 2048];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
        let mut control = Control([0; CONTROL]);
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.0.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = CONTROL as _;
        let length = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
        assert!(length >= 0, "{}", io::Error::last_os_error());
        let mut tos = None;
        let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg.is_null() {
            let data = unsafe { libc::CMSG_DATA(cmsg) };
            match unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type) } {
                (libc::IPPROTO_IP, libc::IP_TOS) => tos = Some(unsafe { *data }),
                (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) =>
                    tos = Some(unsafe { std::ptr::read_unaligned(data as *const libc::c_int) } as u8),
                _ => {}
            }
            cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
        }
        (length as usize, tos.expect("no traffic class told"))
    }

    /// transmit out as quinn sends: waiting for room when there is none.
    async fn send(socket: &Arc<Socket>, poller: &mut Pin<Box<dyn UdpPoller>>, transmit: &Transmit<'_>) {
        loop {
            std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx)).await.unwrap();
            match socket.try_send(transmit) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result.unwrap(),
            }
        }
    }

    fn transmit(to: SocketAddr, ecn: Option<EcnCodepoint>, contents: &[u8]) -> Transmit<'_> {
        Transmit { destination: to, ecn, contents, segment_size: None, src_ip: None }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn realtime_marks_the_packets_af41_with_quinns_ecn_bits() {
        runtime().block_on(async {
            let observer = observer("127.0.0.1:0".parse().unwrap()).unwrap();
            let to = observer.local_addr().unwrap();
            // Its clock a minute old: a realtime that long ago can be told.
            let since = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
            let marking = Arc::new(Marking { since, last_us: AtomicU64::new(0) });
            let socket = Arc::new(Socket::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
                                              marking.clone()).unwrap());
            let mut poller = socket.clone().create_io_poller();
            let packet = [7u8; 300];
            // No realtime yet: DSCP 0, quinn's ECN bits alone (as quinn-udp's).
            send(&socket, &mut poller, &transmit(to, Some(EcnCodepoint::Ect0), &packet[..100])).await;
            assert_eq!(received(&observer), (100, 0b10));
            marking.realtime();
            for (ecn, tos) in [(Some(EcnCodepoint::Ect0), 0x8a), (None, 0x88), (Some(EcnCodepoint::Ce), 0x8b)] {
                send(&socket, &mut poller, &transmit(to, ecn, &packet[..100])).await;
                assert_eq!(received(&observer), (100, tos), "{ecn:?}");
            }
            // With the source address quinn names (a socket on 0.0.0.0's).
            let from = Transmit { src_ip: Some([127, 0, 0, 1].into()), ..transmit(to, Some(EcnCodepoint::Ect0), &packet) };
            send(&socket, &mut poller, &from).await;
            assert_eq!(received(&observer), (300, 0x8a));
            // A GSO batch: each of its datagrams marked.
            if socket.max_transmit_segments() > 1 {
                let batch = Transmit { segment_size: Some(100), ..from };
                send(&socket, &mut poller, &batch).await;
                for _ in 0..3 {
                    assert_eq!(received(&observer), (100, 0x8a));
                }
            } else {
                eprintln!("no GSO on this system: the GSO batch case skipped");
            }
            // The last realtime a minute back, past MARKED_FOR: DSCP 0 again.
            marking.last_us.store(1, Ordering::Relaxed);
            send(&socket, &mut poller, &transmit(to, Some(EcnCodepoint::Ect0), &packet[..100])).await;
            assert_eq!(received(&observer), (100, 0b10));
        });
    }

    #[test]
    fn marked_on_ipv6_and_on_ipv4_from_a_dual_stack_socket() {
        runtime().block_on(async {
            let marking = Arc::new(Marking::new());
            marking.realtime();
            let packet = [7u8; 100];
            // A socket on [::], as quinn's.
            let bound = match std::net::UdpSocket::bind("[::]:0") {
                Ok(bound) => bound,
                Err(e) => {
                    eprintln!("no IPv6 on this system ({e}): the IPv6 and IPv4-mapped cases skipped");
                    return;
                }
            };
            let mut v6only: libc::c_int = 0;
            let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let got = unsafe { libc::getsockopt(bound.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY,
                                                &mut v6only as *mut libc::c_int as *mut libc::c_void, &mut length) };
            assert_eq!(got, 0, "{}", io::Error::last_os_error());
            let dual = Arc::new(Socket::new(bound, marking.clone()).unwrap());
            let mut poller = dual.clone().create_io_poller();
            // Dual-stack, to an IPv4 address, mapped.
            if v6only == 0 {
                let observer4 = observer("127.0.0.1:0".parse().unwrap()).unwrap();
                let mapped = SocketAddr::new(Ipv4Addr::LOCALHOST.to_ipv6_mapped().into(),
                                             observer4.local_addr().unwrap().port());
                send(&dual, &mut poller, &transmit(mapped, Some(EcnCodepoint::Ect0), &packet)).await;
                assert_eq!(received(&observer4), (100, 0x8a));
            } else {
                eprintln!("[::] sockets IPv6 only on this system (bindv6only): the IPv4-mapped case skipped");
            }
            // IPv6 proper, where the system has it.
            let observer6 = match observer("[::1]:0".parse().unwrap()) {
                Ok(observer6) => observer6,
                Err(e) => {
                    eprintln!("no ::1 on this system ({e}): the IPv6 case skipped");
                    return;
                }
            };
            let to = observer6.local_addr().unwrap();
            send(&dual, &mut poller, &transmit(to, Some(EcnCodepoint::Ect0), &packet)).await;
            assert_eq!(received(&observer6), (100, 0x8a));
            let from = Transmit { src_ip: Some(Ipv6Addr::LOCALHOST.into()), ..transmit(to, None, &packet) };
            send(&dual, &mut poller, &from).await;
            assert_eq!(received(&observer6), (100, 0x88));
        });
    }
}
