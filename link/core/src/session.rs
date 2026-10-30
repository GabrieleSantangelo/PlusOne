//! A session with a paired device: its keys, its channels and their
//! reliable messages, over the paths there are, which may change while the
//! session lasts (the cable plugged in or pulled, the Wi-Fi lost and back).
//!
//! A path: a TLS 1.3 stream (the session's messages, the control
//! channels; input and realtime too while there is no UDP), and, when
//! there is IP under it, a UDP socket or the QUIC connection's datagrams
//! (input and realtime, sealed with the session's keys from the first TLS
//! exporter). A path silent PATH_SUSPECT is suspect (told), and lost
//! after PATH_SILENCE. Each side numbers its reliable frames (the
//! session's messages, the control channels'); a ping says up to which the
//! other's came, and what a lost path did not deliver is sent again on the
//! next, the duplicates dropped.
//!
//! With a peer whose hello says "paths": 1 (as ours does), the session
//! keeps up to MAX_PATHS at once, one of each kind: the carrier, which
//! carries what each side sends, and reserves, pinged each PING_EVERY and
//! nothing else. Each side reads every path (each one's frames numbered
//! from what the other has, at most: the duplicates dropped) and picks its
//! carrier alone: the carrier lost or suspect, the best reserve takes its
//! place at once, what it had not delivered first again on it, no
//! detaching; a better path (rank()) is a reserve first, the carrier's
//! once heard throughout HYSTERESIS, unless its kind failed or went
//! suspect carrying within FLAP_HOLD (then only if no other is left; never
//! the cable, which comes and goes by the user's hand). With
//! no path left, or with a peer from before (one path at a time: a better
//! one coming takes over, the one before is retired), the session waits
//! RESUME_WAIT for another (the dialer makes one: plus-one-link).
//!
//! Threads, none of them busy while nothing moves:
//! - the session's: it alone touches the paths' TLS connections; reads
//!   them all, writes what the others queued on the carrier (woken by an
//!   eventfd), pings each second, takes the paths that come, waits for one
//!   when none is left;
//! - each path's datagrams': opens what arrives and hands it to its
//!   channel (a QUIC path's, on quinn's runtime: quic.rs);
//! - one per channel: what the feature writes, to the other side.

use crate::frame::{self, Decoder};
use crate::node::NodeInner;
use crate::seal::{Opener, Sealer};
use crate::stream::Stream;
use crate::sys::{self, Wake};
use crate::{log, Class};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// A ping each this long, on the stream and over UDP.
pub const PING_EVERY: Duration = Duration::from_secs(1);
/// Nothing from the other side on a path this long, though it pings each
/// PING_EVERY: the path is suspect (still used; told, Event::Path).
pub const PATH_SUSPECT: Duration = Duration::from_millis(1500);
/// Nothing from the other side on a path this long (its stream and its
/// datagrams alike): the path is lost.
pub const PATH_SILENCE: Duration = Duration::from_secs(5);
/// No path this long: the session is over.
pub const RESUME_WAIT: Duration = Duration::from_secs(15);
/// The same with a channel open (the desktop, the speakers...): time for
/// both devices to be moved to another network by hand.
pub const RESUME_WAIT_IN_USE: Duration = Duration::from_secs(60);
/// A path a better one took over from ends once the other side is heard
/// on the new one (Session::heard_on), or at the latest this long after.
const HANDOVER_CAP: Duration = Duration::from_secs(3);
/// With a peer that keeps several paths ("paths": 1 in both hellos): this
/// many at once at most, one of each kind (the carrier and its reserves).
const MAX_PATHS: usize = 3;
/// A reserve better than the carrier takes its place once heard throughout
/// this long (its own pings): not at once (a path that came and went at
/// once carried nothing meanwhile).
const HYSTERESIS: Duration = Duration::from_secs(2);
/// A kind whose path failed, or went suspect while it carried, is not
/// made the carrier again for this long, while another is left (again
/// from each time it goes suspect as a reserve meanwhile): a Wi-Fi coming
/// and going does not take the session along each time. Not the cable
/// (flaps()).
const FLAP_HOLD: Duration = Duration::from_secs(60);
/// A better reserve takes the carrier's place only just heard (this
/// recently: its pings come each PING_EVERY, and the session's thread
/// wakes for each), not up to a second after, maybe a second into its
/// network's silence.
const UPGRADE_HEARD: Duration = Duration::from_millis(300);
/// The carrier suspect, a reserve heard this recently takes its place
/// (each is pinged each PING_EVERY): one as silent as the carrier, nearly,
/// is no better.
const RESERVE_HEARD: Duration = Duration::from_millis(1200);
/// A pong this recent over UDP: datagrams go both ways.
const UDP_PROOF: Duration = Duration::from_secs(3);
/// While UDP is not proven yet, a ping this often (the first ones sooner).
const UDP_PROBE: Duration = Duration::from_millis(200);
/// No room in the UDP socket: this long for some, then the packet is lost
/// (a keyframe's tail where the system caps the socket's room).
const UDP_STALL: Duration = Duration::from_millis(20);
/// Each read and write of a connection's handshake and hellos waits this
/// long at most, and the whole of them HANDSHAKE_WHOLE (a host that
/// trickled its bytes, each in time, held the side that dialed it for
/// ever); the listening side closes a connection no session by then too.
const HANDSHAKE: Duration = Duration::from_secs(5);
pub(crate) const HANDSHAKE_WHOLE: Duration = Duration::from_secs(8);
const OPEN_WAIT: Duration = Duration::from_secs(5);
/// An input or realtime message's largest size: sealed (30 bytes more), it
/// fits a QUIC datagram on a 1500-byte network (QUIC's header, tag and
/// frame take about 40 of the 1452 bytes IP and UDP leave), and a UDP one.
pub const MAX_DATAGRAM: usize = 1380;
/// Realtime over the stream: dropped while this much urgent waits to be
/// sent, at most (more on a stream whose budget drains more in
/// REALTIME_WAIT: Drain::wait_most); on a slow stream, while more than
/// REALTIME_WAIT of it does at the rate the stream takes it (Bluetooth:
/// 256 KiB were seconds).
const STREAM_BUDGET: usize = 256 << 10;
/// The most realtime may wait on any stream (a cable whose prior, from a
/// USB 3 link's speed, is hundreds of MB/s): a keyframe of the largest.
const STREAM_WAIT_MOST: usize = 4 << 20;
const REALTIME_WAIT: Duration = Duration::from_millis(150);
/// The stream's rate is measured over this long, while it is full.
const RATE_WINDOW: Duration = Duration::from_millis(200);
/// A Bluetooth path's budget until its stream is measured full (bytes a
/// second): 0.9 Mbit/s, under what RFCOMM carries (1-2 Mbit/s).
const BT_PRIOR: u64 = 900_000 / 8;
/// A cable's (the USB accessory's), the same, when its stream does not
/// know better (Stream::prior: plus-one-link's, from the speed its USB link
/// was made at; the app's cannot read it): 80 Mbit/s, under what USB 2
/// carries by accessory (17.5 MB/s one way), over what any feature asks
/// (the desktop's 20 Mbit/s). With none, the desktop's bitrate followed its
/// own reports, a third off at each frame lost: 3-11 Mbit/s on the cable,
/// 20 on Wi-Fi. A cable has no congestion, only the other side's reading:
/// its drain is its controller, measured down as the other side reads
/// slowly, and back up toward the prior once it no longer does (PROBE_*).
const USB_PRIOR: u64 = 80_000_000 / 8;
/// A cable's budget measured under its prior, not measured again (not
/// full throughout DRAIN_SPAN) for this long: PROBE_UP times higher, up to
/// the prior, and so on each PROBE_AFTER (the other side read slowly a
/// moment, and reads again: a measure is only made with the stream full,
/// which the apps under the budget never fill again).
const PROBE_AFTER: Duration = Duration::from_secs(2);
const PROBE_UP: f64 = 1.5;
/// A cable's budget gone up (PROBE_UP) and measured back down (the other
/// side reads no faster): up again twice as long after, and so on up to
/// this; each PROBE_AFTER again once a time up holds (not measured back
/// down PROBE_AFTER on). (A tablet reading slower than the desktop asked
/// had its budget 1.5 times over every 4.4 s, realtime dropped for 0.8 s
/// each time, for as long as it lasted.)
const PROBE_MOST: Duration = Duration::from_secs(32);
/// That interval said in the log as it changes, this often at most.
const PROBE_SAID: Duration = Duration::from_secs(10);
/// What a stream drains is measured over the last this long it was full
/// throughout: its socket's room, either way, is then a few percent of it
/// (of a window's alone, as much as half).
const DRAIN_SPAN: Duration = Duration::from_secs(1);
/// A stream's budget is never told under this (bytes a second, 8 kbit/s):
/// 0 is none to the apps.
const DRAIN_LEAST: f64 = 1000.0;
/// Reliable frames handed to TLS at a time: an urgent frame (input,
/// realtime, a ping) queued meanwhile waits behind no more than this.
const WRITE_SLICE: usize = 16 << 10;
/// The datagrams' DSCP, AF41 (34), as the IP header's byte.
const REALTIME_TOS: libc::c_int = 34 << 2;
/// Room in the UDP socket, each way: a keyframe's packets at once.
const UDP_BUFFER: libc::c_int = 4 << 20;
/// Room in a channel's socket, each way (the system caps it: a control
/// message is at most about 200 KB; more goes on a bulk channel).
const CHANNEL_BUFFER: usize = 1 << 20;
/// A control message with no room in its channel: this long for the
/// feature to read, then the channel is closed (it holds up the stream).
const CONTROL_STALL: Duration = Duration::from_secs(1);
/// A control channel's window, with a peer that says "flow" in its hello:
/// what its sender may have sent beyond what the other side's feature has
/// taken. The receiving side keeps what comes in a queue of its own (never
/// blocking the session, never closing a channel for being slow), and says
/// how much its feature has taken ("credit", a running total) each
/// GRANT_EVERY. A control message is at most about 200 KB (the sockets'
/// own limit): far under WINDOW - GRANT_EVERY, so a sender never waits on
/// credit that is held back.
const WINDOW: u64 = 1 << 20;
const GRANT_EVERY: u64 = 64 << 10;
/// A control message waits for the queue to drain below this before it
/// joins it: a big transfer (a PDF, the app) feeds the stream as fast as
/// the stream goes, never ahead of it, so what shares the stream (the
/// cable's realtime packets, under STREAM_BUDGET) never waits behind more.
const CONTROL_BACKLOG: usize = 128 << 10;
/// Channels a session may have at once (architecture §3.13); one more is
/// refused, either side's.
const MAX_CHANNELS: usize = 64;
/// Realtime counts as going this long after its last datagram (pace.rs).
const REALTIME_GOING: Duration = Duration::from_millis(500);
/// The path's pings this often while realtime and bulk go together
/// (pace.rs's samples), not each PING_EVERY.
const PROBE_BUSY: Duration = Duration::from_millis(40);
/// Reliable frames taken since the last ping that make one go at once.
const ACK_AFTER: u64 = 32;
/// Reliable bytes sent and not yet acknowledged, at most: past it no more
/// go until the other side says it has them (it acknowledges each
/// ACK_AFTER frames, 32 MiB at the very most; one that never does holds no
/// more of ours than this).
const UNACKED_MOST: usize = 64 << 20;
/// The session's own messages waiting (Queue::Ahead), at most, once the
/// other side's call for more: past it the path is lost (one that sends
/// opens and takes nothing would pile up their answers for ever; ours
/// alone are a few KiB, credits and opens of 64 channels at most).
const AHEAD_MOST: usize = 256 << 10;
const KEYS_LABEL: &[u8] = b"EXPORTER-plus-one-link datagrams";
const ID_LABEL: &[u8] = b"EXPORTER-plus-one-link session";
const DATAGRAM_PATH: u16 = 0;  // the datagrams' own channel: pings and pongs
const PING: u8 = 1;
const PONG: u8 = 2;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// The channel a message of the other side's names: a u16, or an error
/// (a bigger number must not wrap round onto a channel that is one).
fn channel_id(message: &Value) -> Result<u16, String> {
    let id = &message["id"];
    id.as_u64().and_then(|id| u16::try_from(id).ok()).ok_or_else(|| format!("no channel's id: {id}"))
}

/// Whether a kind's path comes and goes by itself (the air's: a Wi-Fi, a
/// Bluetooth link; the relay's Internet): FLAP_HOLD holds it. Not the
/// cable: pulled, it was the user's hand, not a flaky link, and plugged
/// back in it is the carrier as soon as it is heard throughout HYSTERESIS
/// (held 60 s after each pull, it stayed a reserve while the user plugged
/// it again and again).
fn flaps(kind: &str) -> bool {
    kind != "usb"
}

/// How good a path is: a better one takes over from a worse.
fn rank(kind: &str) -> u8 {
    match kind {
        "usb" => 4,
        "p2p" | "ap" => 3,  // Wi-Fi Direct, the PC's own access point: once through the air, no one else's queue
        "lan" => 2,
        "bt" => 1,  // Bluetooth: slow, but with no network at all
        _ => 0,     // the relay
    }
}

/// A channel's end, the feature's: a message socket. Close it (drop) and
/// the channel closes; its end (a read of 0) is the channel closed by the
/// other side, or the session over.
pub struct Channel {
    fd: OwnedFd,
    pub id: u16,
    pub class: Class,
    pub feature: String,
    pub name: String,
}

impl Channel {
    /// The same channel, for another thread (one reads while one writes).
    pub fn try_clone(&self) -> io::Result<Channel> {
        Ok(Channel { fd: self.fd.try_clone()?, id: self.id, class: self.class, feature: self.feature.clone(),
                     name: self.name.clone() })
    }

    /// One message, waiting for room.
    pub fn send(&self, data: &[u8]) -> io::Result<()> {
        sys::send(self.fd.as_raw_fd(), data, true).map(|_| ())
    }

    /// One message into buf: Some(its length), Some(0) at the end, None if
    /// nothing came within timeout.
    pub fn recv(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<Option<usize>> {
        if !sys::readable(self.fd.as_raw_fd(), timeout)? {
            return Ok(None);
        }
        let length = sys::recv(self.fd.as_raw_fd(), buf)?;
        if length > buf.len() {
            return Err(invalid(format!("a message of {length} bytes")));
        }
        Ok(Some(length))
    }

    /// The descriptor, for whoever reads and writes it now (plus-one-link
    /// hands it to an app).
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsRawFd for Channel {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// A channel the other side opened, for a feature we serve.
pub struct Incoming {
    pub session: Arc<Session>,
    pub channel: Channel,
}

/// The session's end of a channel.
struct End {
    id: u16,
    class: Class,
    fd: OwnedFd,
    /// Input: the newest datagram's counter handed on, plus one (0: none).
    newest: AtomicU64,
    /// Closed: a pump waiting for credit gives up (End::gone).
    gone: AtomicBool,
    /// Flow (control, with a peer that has it): bytes sent on it, and the
    /// other side's running total taken by its feature; a pump waiting for
    /// credit sleeps on credit until that moves, or the channel goes.
    sent: AtomicU64,
    granted: Mutex<u64>,
    credit: Condvar,
    /// Flow: what comes for it, to its feature in order on a thread of its
    /// own (None: the channel's end, after what came before it); bytes
    /// come, and taken by the feature.
    inbox: Mutex<Option<mpsc::Sender<Option<Vec<u8>>>>>,
    received: AtomicU64,
    taken: AtomicU64,
}

impl End {
    fn new(id: u16, class: Class, fd: OwnedFd) -> End {
        End { id, class, fd, newest: AtomicU64::new(0), gone: AtomicBool::new(false), sent: AtomicU64::new(0),
              granted: Mutex::new(0), credit: Condvar::new(), inbox: Mutex::new(None), received: AtomicU64::new(0),
              taken: AtomicU64::new(0) }
    }

    /// Closed (by either side, or with the session): its pump, if it waits
    /// for credit, gives up.
    fn gone(&self) {
        self.gone.store(true, Ordering::Relaxed);
        let _granted = self.granted.lock().unwrap();
        self.credit.notify_all();
    }
}

/// A reliable frame not numbered yet: it is numbered as it leaves
/// (Outbox::take), in the order frames leave, so the queue may put one
/// ahead of others (the other side takes numbers in order only).
struct Waiting {
    kind: u8,
    channel: u16,
    body: Vec<u8>,
}

impl Waiting {
    fn size(&self) -> usize {
        frame::HEADER + 8 + self.body.len()
    }
}

/// Where a reliable frame waits.
#[derive(Clone, Copy)]
enum Queue {
    /// The session's own messages (opens and their answers, credits): ahead
    /// of every channel's, never behind a transfer (an opened channel went
    /// unanswered behind seconds of one on a slow path).
    Ahead,
    /// A channel's: in its order, the channels in turn (a message on one
    /// waits behind no other's transfer); a channel's close after its data.
    Channel(u16),
    /// After everything ("bye").
    Last,
}

/// What waits to go on the stream: the urgent (input and realtime frames,
/// pings) never wait behind the reliable (control messages, a big
/// transfer's), which go to TLS frame by frame, a slice at a time; between
/// two slices the urgent go first.
#[derive(Default)]
struct Outbox {
    urgent: Vec<u8>,
    /// Numbered already: what a lost path had not delivered, first again.
    resend: VecDeque<Vec<u8>>,
    ahead: VecDeque<Waiting>,
    ahead_bytes: usize,
    channels: BTreeMap<u16, (VecDeque<Waiting>, usize)>,
    last: VecDeque<Waiting>,
    /// The channel whose turn comes next.
    turn: u16,
    reliable_bytes: usize,
    /// A channel's queue went under CONTROL_BACKLOG since the last look:
    /// its pump may wait for that (Session::room).
    made_room: bool,
}

impl Outbox {
    fn len(&self) -> usize {
        self.urgent.len() + self.reliable_bytes
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What one channel has waiting (bytes).
    fn channel_len(&self, id: u16) -> usize {
        self.channels.get(&id).map_or(0, |(_, bytes)| *bytes)
    }

    fn push(&mut self, waiting: Waiting, queue: Queue) {
        self.reliable_bytes += waiting.size();
        match queue {
            Queue::Ahead => {
                self.ahead_bytes += waiting.size();
                self.ahead.push_back(waiting);
            }
            Queue::Last => self.last.push_back(waiting),
            Queue::Channel(id) => {
                let (frames, bytes) = self.channels.entry(id).or_default();
                *bytes += waiting.size();
                frames.push_back(waiting);
            }
        }
    }

    /// A new path: the urgent are stale; what the last one had not
    /// delivered (numbered) goes first again, the rest as it was.
    fn restart(&mut self, unacked: impl Iterator<Item = Vec<u8>>) {
        self.reliable_bytes -= self.resend.iter().map(Vec::len).sum::<usize>();
        self.urgent.clear();
        self.resend = unacked.collect();
        self.reliable_bytes += self.resend.iter().map(Vec::len).sum::<usize>();
    }

    /// The next channel's frame, the channels in turn.
    fn next_channel(&mut self) -> Option<Waiting> {
        let id = self.channels.range(self.turn..).chain(self.channels.range(..self.turn)).map(|(id, _)| *id).next()?;
        let (frames, bytes) = self.channels.get_mut(&id)?;
        let waiting = frames.pop_front()?;
        self.made_room |= *bytes > CONTROL_BACKLOG && *bytes - waiting.size() <= CONTROL_BACKLOG;
        *bytes -= waiting.size();
        if frames.is_empty() {
            self.channels.remove(&id);
        }
        self.turn = id.wrapping_add(1);
        Some(waiting)
    }

    /// The next for TLS: every urgent frame and those to send again, then
    /// reliable frames up to reliable bytes (0: none now) to be numbered;
    /// and how many bytes of the reliable.
    fn take(&mut self, reliable: usize) -> (Vec<u8>, Vec<Waiting>, usize) {
        let mut out = std::mem::take(&mut self.urgent);
        let (mut fresh, mut taken) = (Vec::new(), 0);
        while taken < reliable {
            if let Some(bytes) = self.resend.pop_front() {
                taken += bytes.len();
                self.reliable_bytes -= bytes.len();
                out.extend_from_slice(&bytes);
                continue;
            }
            let next = self.ahead.pop_front().inspect(|waiting| self.ahead_bytes -= waiting.size())
                .or_else(|| self.next_channel()).or_else(|| self.last.pop_front());
            let Some(waiting) = next else { break };
            taken += waiting.size();
            self.reliable_bytes -= waiting.size();
            fresh.push(waiting);
        }
        (out, fresh, taken)
    }
}

/// The reliable frames: ours numbered, kept until acknowledged; the
/// other side's newest taken.
#[derive(Default)]
struct Reliable {
    next: u64,
    unacked: VecDeque<(u64, Vec<u8>)>,
    unacked_bytes: usize,
    heard: u64,
    acknowledged: u64,  // the newest of theirs we said we have
}

/// A path's state (LINK-STATES §4): the carrier, heard lately; nothing
/// heard for PATH_SUSPECT (still used); lost (PATH_SILENCE, its stream's
/// error or end), its last; a reserve, heard lately (a peer that keeps
/// several paths: pinged, nothing else); let go of on purpose, its last
/// (a better one of its kind took its place, one too many, or the other
/// side let go of it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathState {
    Active,
    Suspect,
    Failed,
    Standby,
    Closed,
}

impl PathState {
    pub fn name(self) -> &'static str {
        match self {
            PathState::Active => "active",
            PathState::Suspect => "suspect",
            PathState::Failed => "failed",
            PathState::Standby => "standby",
            PathState::Closed => "closed",
        }
    }

    fn from_u8(state: u8) -> PathState {
        match state {
            1 => PathState::Suspect,
            2 => PathState::Failed,
            3 => PathState::Standby,
            4 => PathState::Closed,
            _ => PathState::Active,
        }
    }
}

/// A path as it is now (Session::paths, Event::Path).
#[derive(Clone, Debug)]
pub struct PathInfo {
    /// The session's number for it: a path taken again is another one.
    pub id: u32,
    /// "usb", "lan", "p2p", "ap", "bt", "relay".
    pub kind: String,
    /// What carries it (Session::transport).
    pub transport: &'static str,
    pub state: PathState,
    /// The last round trip over its datagrams, if they work.
    pub rtt_ms: Option<f32>,
    /// Since something of the other side's last came on it.
    pub silent_ms: u32,
    /// What its congestion controller allows now (bits a second), and
    /// whose: "gcc" (QUIC's, cc.rs), "drain" (what its stream drains,
    /// measured), "prior" (its kind's, not measured yet), "none".
    pub budget_bps: Option<u64>,
    pub budget_from: &'static str,
}

impl PathInfo {
    /// As plus-one-link and the app's core tell it: devices' "paths", the
    /// "path" event.
    pub fn json(&self) -> Value {
        json!({"id": self.id, "kind": self.kind, "transport": self.transport, "state": self.state.name(),
               "rtt_ms": self.rtt_ms.map(|rtt| (rtt as f64 * 100.0).round() / 100.0), "silent_ms": self.silent_ms,
               "budget_bps": self.budget_bps, "budget_from": self.budget_from})
    }
}

/// A path the session runs on now.
struct Path {
    /// PathInfo::id.
    id: u32,
    kind: String,
    stream: Box<dyn Stream>,  // a clone of the session thread's, to shut it down
    udp: Option<UdpSocket>,
    udp_peer: Mutex<Option<SocketAddr>>,
    /// A QUIC path's connection: its datagrams instead of UDP's.
    quic: Option<crate::quic::Quic>,
    /// What carries it (Session::transport): "quic", "tcp", "stream".
    transport: &'static str,
    started: Instant,
    heard_us: AtomicU64,   // since started: the stream's last frame, or an authenticated datagram
    proven_us: AtomicU64,  // a pong's (0: none yet)
    rtt_us: AtomicU64,
    realtime_us: AtomicU64,  // since started: the last realtime or input datagram sent (0: none)
    /// Not the session's path any more (lost, or a better one took over;
    /// with a peer that keeps several, out of them).
    gone: AtomicBool,
    /// A better one of its kind took its place: it ends once the other
    /// side is heard on that one (a peer that keeps several paths).
    leaving: AtomicBool,
    /// Shut down (end_path): its datagrams are read until then, after a
    /// handover too (they are sealed with the session's keys, replays
    /// dropped by the opener's window).
    ended: AtomicBool,
    /// Its PathState, as a number: the session's thread alone moves it.
    state: AtomicU8,
    /// A stream's budget (Drain; bytes a second, 0: none), its kind's
    /// prior from the start (told with Up or Moved), and whether it was
    /// measured since.
    budget: AtomicU64,
    drained: AtomicBool,
    /// Its prior (bytes a second, 0: none): a probing kind's budget goes
    /// back up to it (Drain::probing).
    prior: u64,
}

/// A path's end: its stream shut down, its QUIC connection closed, its
/// UDP socket shut down (its thread, asleep in a read, wakes to it).
fn end_path(path: &Path) {
    path.ended.store(true, Ordering::SeqCst);
    path.stream.shutdown();
    if let Some(quic) = &path.quic {
        quic.close();
    }
    if let Some(udp) = &path.udp {
        sys::shutdown(udp.as_raw_fd());
    }
}

impl Path {
    fn us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    fn heard(&self) {
        self.heard_us.store(self.us().max(1), Ordering::Relaxed);
    }

    /// Since something of the other side's last came on it (µs).
    fn silent(&self) -> u64 {
        self.us().saturating_sub(self.heard_us.load(Ordering::Relaxed))
    }

    fn state(&self) -> PathState {
        PathState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// Realtime or input went on it within the last REALTIME_GOING.
    fn realtime_going(&self) -> bool {
        let last = self.realtime_us.load(Ordering::Relaxed);
        last != 0 && self.us().saturating_sub(last) < REALTIME_GOING.as_micros() as u64
    }

    fn udp_works(&self) -> bool {
        let proven = self.proven_us.load(Ordering::Relaxed);
        (self.udp.is_some() || self.quic.is_some()) && proven != 0
            && self.us().saturating_sub(proven) < UDP_PROOF.as_micros() as u64
    }

    /// What its congestion controller allows now (bytes a second), and
    /// whose (PathInfo::budget_from).
    fn budget(&self) -> (Option<u64>, &'static str) {
        match (&self.quic, self.budget.load(Ordering::Relaxed)) {
            (Some(quic), _) => (Some(quic.rate()), "gcc"),
            (None, 0) => (None, "none"),
            (None, rate) => (Some(rate), if self.drained.load(Ordering::Relaxed) { "drain" } else { "prior" }),
        }
    }

    /// How good it is: one higher takes over from one lower (QUIC over TCP
    /// at the same kind).
    fn rank(&self) -> (u8, bool) {
        (rank(&self.kind), self.quic.is_some())
    }

    fn info(&self) -> PathInfo {
        let (budget, from) = self.budget();
        PathInfo { id: self.id, kind: self.kind.clone(), transport: self.transport, state: self.state(),
                   rtt_ms: self.udp_works().then(|| self.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0),
                   silent_ms: (self.silent() / 1000).min(u32::MAX as u64) as u32,
                   budget_bps: budget.map(|rate| rate * 8), budget_from: from }
    }
}

/// What a stream path of kind is taken to carry until it is measured
/// (bytes a second): Bluetooth's, a cable's; none for the others (TCP is
/// paced: pace.rs).
fn prior(kind: &str) -> Option<u64> {
    match kind {
        "bt" => Some(BT_PRIOR),
        "usb" => Some(USB_PRIOR),
        _ => None,
    }
}

/// Whether a kind's budget, measured under its prior, goes back up toward
/// it (PROBE_AFTER): a cable's, whose drain is the other side's reading
/// alone. Not Bluetooth's: the air's, its measure is what it carries.
fn probes(kind: &str) -> bool {
    kind == "usb"
}

/// What a stream takes, measured each RATE_WINDOW from what its socket
/// took: how much realtime may wait on it (REALTIME_WAIT of what it takes
/// while it is full: what its socket took, full throughout; full at some
/// point only, that or its budget if more; at most wait_most()), and, for a
/// kind with a prior (Bluetooth, a cable), its
/// budget, told as a QUIC path's controller tells its own
/// (cc.rs: past PUBLISH_CHANGE of the last told, a cut at once, up at most
/// each PUBLISH_EVERY). The budget is what the stream drains: what its
/// socket took over the last DRAIN_SPAN of windows, one after the other,
/// each full from start to end with more waiting all along (then what the
/// socket took is what the path carried, give or take the socket's room;
/// its room filling, or a window it ran dry in, says nothing of the
/// path); the prior until then, and never more than the path drained
/// once measured (a measure under the prior wins).
struct Drain {
    /// The window: its start, what the socket took in it, full at some
    /// point in it, full as it began, full at the last write, run dry
    /// (nothing waiting) at some point in it, and whether it may end now
    /// (the socket full, or nothing waiting).
    start: Instant,
    bytes: usize,
    full: bool,
    full_at_start: bool,
    full_now: bool,
    dry: bool,
    settled: bool,
    /// The full windows one after the other, their bytes and lengths, the
    /// last DRAIN_SPAN of them.
    run: VecDeque<(usize, Duration)>,
    /// Bytes a second (None: no prior, never told), measured or not; the
    /// last told, and when.
    budget: Option<u64>,
    measured: bool,
    told: u64,
    told_at: Instant,
    /// A kind that probes (probes()): its prior, the budget goes back up to
    /// it; when it next does, measured under it; how long after each time
    /// (PROBE_AFTER to PROBE_MOST); what the last time took it up to, until
    /// it held or was measured back down; that interval last said, and when.
    ceiling: Option<u64>,
    probe_at: Instant,
    probe_every: Duration,
    probed: Option<u64>,
    said_every: Duration,
    said_at: Option<Instant>,
}

impl Drain {
    fn new(now: Instant, prior: Option<u64>) -> Drain {
        Drain { start: now, bytes: 0, full: false, full_at_start: false, full_now: false, dry: false, settled: false,
                run: VecDeque::new(), budget: prior, measured: false, told: prior.unwrap_or(0), told_at: now,
                ceiling: None, probe_at: now, probe_every: PROBE_AFTER, probed: None, said_every: PROBE_AFTER, said_at: None }
    }

    /// As one of a kind that probes: back up to ceiling (its prior) once
    /// measured under it and not measured again for PROBE_AFTER (or longer:
    /// PROBE_MOST).
    fn probing(mut self, ceiling: u64) -> Drain {
        self.ceiling = Some(ceiling).filter(|ceiling| *ceiling != 0);
        self
    }

    /// How long its budget waits between times up, when it changed since
    /// last said and PROBE_SAID after that at the earliest (for the log).
    fn probe_note(&mut self, now: Instant) -> Option<Duration> {
        if self.probe_every == self.said_every || self.said_at.is_some_and(|at| now.duration_since(at) < PROBE_SAID) {
            return None;
        }
        (self.said_every, self.said_at) = (self.probe_every, Some(now));
        Some(self.probe_every)
    }

    fn wrote(&mut self, bytes: usize) {
        self.bytes += bytes;
    }

    /// How much realtime may wait on the stream at most: STREAM_BUDGET, or
    /// REALTIME_WAIT of its budget if more (a cable drains a keyframe at the
    /// desktop's bitrate, more than STREAM_BUDGET, in a few ms), up to
    /// STREAM_WAIT_MOST.
    fn wait_most(&self) -> usize {
        let budget = self.budget.map_or(0, |budget| (budget as f64 * REALTIME_WAIT.as_secs_f64()) as usize);
        STREAM_BUDGET.max(budget.min(STREAM_WAIT_MOST))
    }

    /// A round's writes done: the socket full (more waits it would not
    /// take), or nothing waiting at all; neither: more goes at once.
    fn after(&mut self, full: bool, dry: bool) {
        self.full |= full;
        self.full_now = full;
        self.dry |= dry;
        self.settled = full || dry;
    }

    /// Past RATE_WINDOW, the window taken and the next begun: how much
    /// realtime may wait now, and the budget to tell if it moved. Not in
    /// a round that emptied TLS's buffer into the socket with more to go
    /// at once: the socket all but empty, the window would be its room
    /// short (and one as long as two of the socket's refills ended in
    /// such a round each time: never a measure).
    fn roll(&mut self, now: Instant) -> Option<(usize, Option<u64>)> {
        let elapsed = now.duration_since(self.start);
        if elapsed < RATE_WINDOW || !self.settled {
            return None;
        }
        // Full throughout: what its socket took is what it takes. Full at
        // some point only: that at least, its budget if more (a stream much
        // faster than what goes on it, a cable, fills its little room at
        // each burst all the same: what its socket took was only what it
        // was given, and REALTIME_WAIT of that, under a keyframe, dropped
        // the keyframe's tail each time, the tablet asked for another, in a
        // loop). Else all there is. (Its budget, its prior until measured,
        // with the stream full throughout: a cable the other side read at
        // 1 MB/s let 1.8 MB of realtime wait, its prior's REALTIME_WAIT,
        // 1.8 s of what it drained.)
        let throughout = self.full_at_start && self.full_now && !self.dry;
        let wait = if self.full {
            let took = self.bytes as f64 / elapsed.as_secs_f64();
            let takes = if throughout { took } else { took.max(self.budget.unwrap_or(0) as f64) };
            ((takes * REALTIME_WAIT.as_secs_f64()) as usize).clamp(8 << 10, self.wait_most())
        } else {
            self.wait_most()
        };
        let mut told = None;
        if self.budget.is_some() {
            if throughout {
                self.run.push_back((self.bytes, elapsed));
            } else {
                self.run.clear();  // what the socket held meanwhile is not known: from the next again
            }
            let mut span: Duration = self.run.iter().map(|(_, length)| *length).sum();
            while self.run.front().is_some_and(|(_, length)| span - *length >= DRAIN_SPAN) {
                span -= self.run.pop_front().map_or(Duration::ZERO, |(_, length)| length);
            }
            let mut budget = None;
            if span >= DRAIN_SPAN {
                let bytes: usize = self.run.iter().map(|(bytes, _)| *bytes).sum();
                let measured = (bytes as f64 / span.as_secs_f64()).max(DRAIN_LEAST) as u64;
                budget = Some(measured);
                self.measured = true;
                // Gone up and measured back down: the next time up twice as
                // long after; measured as high as it went: it held.
                if let Some(went) = self.probed.take() {
                    self.probe_every = if measured < went { (self.probe_every * 2).min(PROBE_MOST) } else { PROBE_AFTER };
                }
                self.probe_at = now + self.probe_every;
            } else if let (Some(ceiling), Some(was)) = (self.ceiling, self.budget) {
                // Not measured again for its interval: the last time up held,
                // and up again toward the prior. Not while it is full
                // throughout (a measure under way: up then, the apps would
                // only fill it more).
                if now >= self.probe_at && self.run.is_empty() {
                    if self.probed.take().is_some() {
                        self.probe_every = PROBE_AFTER;
                    }
                    if was < ceiling {
                        let up = ((was as f64 * PROBE_UP) as u64).min(ceiling);
                        budget = Some(up);
                        self.measured = up < ceiling;
                        // Held if not measured back down by PROBE_AFTER (a
                        // measure takes DRAIN_SPAN): then up again at once.
                        self.probed = Some(up);
                        self.probe_at = now + PROBE_AFTER;
                    }
                }
            }
            if let Some(budget) = budget {
                self.budget = Some(budget);
                let moved = budget.abs_diff(self.told) as f64 > crate::cc::PUBLISH_CHANGE * self.told as f64;
                if moved && (budget < self.told || now.duration_since(self.told_at) >= crate::cc::PUBLISH_EVERY) {
                    (self.told, self.told_at) = (budget, now);
                    told = Some(budget);
                }
            }
        }
        self.start = now;
        self.bytes = 0;
        self.full = false;
        self.full_at_start = self.full_now;
        self.dry = false;
        Some((wait, told))
    }
}

/// A path as the session's thread drives it: its TLS connection and its
/// stream (it alone touches them), what came on it and is not a whole
/// frame yet, what its stream drains (the carrier's measured), its pings
/// due, when it went suspect, since when it has been heard throughout (a
/// reserve takes the carrier's place HYSTERESIS after), when its stream
/// last took something, how the last round left it, and, if a better one
/// of its kind took its place, that one and when it ends at the latest.
struct Live {
    path: Arc<Path>,
    conn: rustls::Connection,
    stream: Box<dyn Stream>,
    decoder: Decoder,
    drain: Drain,
    next_ping: Instant,
    next_udp_ping: Instant,
    suspect_since: Option<Instant>,
    steady_since: Instant,
    taken_at: Instant,
    blocked: bool,
    took: bool,
    leaving: Option<(Arc<Path>, Instant)>,
}

/// What the session's thread keeps of its paths: those it drives, the
/// carrier first; whether one was taken before (the next carrier sends
/// again what that one had not delivered, and tells Moved); the last
/// carrier lost and why, until it is said (detached, or another taken);
/// when each kind last failed, or went suspect carrying (FLAP_HOLD).
struct Paths {
    lives: Vec<Live>,
    resumed: bool,
    last: Option<(String, String)>,
    held: HashMap<String, Instant>,
}

impl Paths {
    /// Whether a path of kind is held off the carrier's place (FLAP_HOLD).
    fn held(&self, kind: &str) -> bool {
        self.held.get(kind).is_some_and(|at| at.elapsed() < FLAP_HOLD)
    }

    /// kind held off the carrier's place from at (FLAP_HOLD), if it flaps.
    fn hold(&mut self, kind: &str, at: Instant) {
        if flaps(kind) {
            self.held.insert(kind.to_owned(), at);
        }
    }
}

/// Why a path left the session's thread.
enum Lost {
    /// Its stream's error or end, its silence: failed.
    Failed(String),
    /// The other side let go of it (TLS's close_notify).
    Notified,
    /// Let go of here (a better one of its kind took its place, one too many).
    LetGo(&'static str),
}

impl Lost {
    fn reason(&self) -> String {
        match self {
            Lost::Failed(reason) => reason.clone(),
            Lost::Notified => "the other side closed".into(),
            Lost::LetGo(why) => (*why).into(),
        }
    }
}

/// A path's stream after what came on it was taken (Session::take_in).
enum In {
    Open,
    /// Its end.
    Ended,
    /// TLS's close_notify: the other side let go of it on purpose.
    Notified,
}

pub struct Session {
    /// The other side's certificate's fingerprint, and what its hello said.
    pub peer: String,
    pub name: String,
    /// The session's, the same on both sides: a path for it names it.
    pub id: String,
    features: Mutex<Value>,
    node: Weak<NodeInner>,
    closed: AtomicBool,
    /// Said "bye": the path's end that follows is not a path lost.
    closing: AtomicBool,
    channels: Mutex<HashMap<u16, Arc<End>>>,
    /// Channels the other side closed whose inbox still hands its feature
    /// what came before: shut with the session too (end()).
    draining: Mutex<Vec<Weak<End>>>,
    pending: Mutex<HashMap<u16, mpsc::Sender<Result<(), String>>>>,
    next_id: AtomicU16,
    /// The other side has flow control on control channels (WINDOW).
    flow: bool,
    /// The reliable frames' pace on the current path (pace.rs).
    pace: Mutex<crate::pace::Pace>,
    /// How much urgent may wait on the stream before realtime is dropped.
    stream_budget: std::sync::atomic::AtomicUsize,
    outbox: Mutex<Outbox>,
    /// A channel's queue under CONTROL_BACKLOG again, or the session over:
    /// a pump waiting for room (on the outbox's lock) goes on.
    room: Condvar,
    reliable: Mutex<Reliable>,
    wake: Wake,
    /// The path the session runs on: the carrier.
    path: Mutex<Option<Arc<Path>>>,
    /// Every path the session's thread drives now, the carrier first (it
    /// alone with a peer from before): Session::paths, adopt()'s choice.
    live: Mutex<Vec<Arc<Path>>>,
    /// The other side keeps several paths ("paths": 1 in its hello).
    multipath: bool,
    /// With a peer from before: the path that takes over next.
    arriving: Mutex<Option<Established>>,
    /// With a peer that keeps several: the paths adopted, for the session's
    /// thread to take (a reserve, or the carrier's place).
    joining: Mutex<Vec<Established>>,
    /// The path a handover left, and when it ends at the latest (retire()).
    retiring: Mutex<Option<(Arc<Path>, Instant)>>,
    retiring_any: AtomicBool,
    /// The next path's PathInfo::id.
    next_path: AtomicU32,
    sealer: Sealer,
    opener: Mutex<Opener>,
    dropped: AtomicU64,
}

/// What a connection's TLS handshake and hellos left: a new session
/// starts from it, or a session takes it as its path.
pub(crate) struct Established {
    conn: rustls::Connection,
    stream: Box<dyn Stream>,
    decoder: Decoder,
    pub(crate) peer: String,
    hello: Value,
    udp: Option<UdpSocket>,
    udp_peer: Option<SocketAddr>,
    sealer: Sealer,
    opener: Opener,
    dialer: bool,
    kind: String,
    id: String,
    /// The session both hellos named (the same one): this is a path for it.
    pub(crate) resumes: Option<String>,
    /// The other side has no path for it now: this one is taken, whatever
    /// we believe of ours (it is gone for them).
    forced: bool,
    /// A QUIC path's connection (quic.rs).
    quic: Option<crate::quic::Quic>,
}

impl Established {
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    /// The connection under the stream is QUIC's: its datagrams carry ours.
    pub(crate) fn set_quic(&mut self, quic: crate::quic::Quic) {
        self.quic = Some(quic);
    }

    /// Where to make a QUIC path to the other side, if this is a TCP one we
    /// made and its hello says it takes QUIC.
    pub(crate) fn quic_offer(&self) -> Option<SocketAddr> {
        let port = self.hello["quic"].as_u64().filter(|p| (1..=65535).contains(p))?;
        match (self.dialer, &self.quic, self.stream.addresses()) {
            (true, None, Some((peer, _))) if peer.is_ipv4() => Some(SocketAddr::new(peer, port as u16)),
            _ => None,
        }
    }

    fn close(&self) {
        self.stream.shutdown();
        if let Some(quic) = &self.quic {
            quic.close();
        }
    }

    /// As close(), TLS's close_notify first (not waiting for room): a peer
    /// that keeps several paths takes its end for a path let go of, not
    /// one failed (FLAP_HOLD).
    fn close_notified(mut self) {
        self.conn.send_close_notify();
        if self.stream.set_nonblocking(true).is_ok() {
            while self.conn.wants_write() && self.conn.write_tls(&mut self.stream).is_ok_and(|n| n > 0) {}
        }
        self.close();
    }
}

/// A connection's stream during its handshake and hellos: none of its
/// reads and writes goes past the instant (HANDSHAKE_WHOLE).
struct Until<'a>(&'a mut Box<dyn Stream>, Instant);

impl Until<'_> {
    /// The stream's timeouts down to the time left, once under HANDSHAKE.
    fn left(&mut self) -> io::Result<()> {
        let left = self.1.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no session in time"));
        }
        if left < HANDSHAKE {
            self.0.set_timeouts(Some(left))?;
        }
        Ok(())
    }
}

impl Read for Until<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.left()?;
        self.0.read(buf)
    }
}

impl Write for Until<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.left()?;
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The TLS handshake, the keys and the hellos, on a new connection. The
/// verifier has checked the other side is paired. kind: what it runs on;
/// ours: the session we have with that device, if any (to go on with it:
/// both hellos name it, or a new one starts), and whether it has no path.
pub(crate) fn establish(mut conn: rustls::Connection, mut stream: Box<dyn Stream>, dialer: bool, hello: Value,
                        kind: &str, ours: &dyn Fn(&str) -> Option<(String, bool)>) -> io::Result<Established> {
    stream.set_timeouts(Some(HANDSHAKE))?;
    let until = Instant::now() + HANDSHAKE_WHOLE;
    while conn.is_handshaking() {
        conn.complete_io(&mut Until(&mut stream, until))?;
    }
    let peer = conn.peer_certificates().and_then(|c| c.first()).map(|c| crate::fingerprint(c))
        .ok_or_else(|| invalid("no certificate"))?;
    let keys = conn.export_keying_material([0u8; 64], KEYS_LABEL, None).map_err(|e| invalid(e.to_string()))?;
    let id_bytes = conn.export_keying_material([0u8; 16], ID_LABEL, None).map_err(|e| invalid(e.to_string()))?;
    let id: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let (mine, theirs) = if dialer { (0, 32) } else { (32, 0) };
    let (send_direction, receive_direction) =
        if dialer { (crate::seal::DIALER, crate::seal::LISTENER) } else { (crate::seal::LISTENER, crate::seal::DIALER) };
    let sealer = Sealer::new(keys[mine..mine + 32].try_into().unwrap(), send_direction);
    let opener = Opener::new(keys[theirs..theirs + 32].try_into().unwrap(), receive_direction);

    // Datagrams as UDP when there is IP under the stream.
    let udp = match stream.addresses() {
        Some((_, local)) => {
            let udp = UdpSocket::bind(SocketAddr::new(local, 0))?;
            // Input and realtime ahead of the stream's bulk on the air: DSCP
            // AF41, which Wi-Fi puts in its video queue (WMM AC_VI, RFC
            // 8325); the stream stays best effort.
            if local.is_ipv4() {
                sys::set_int(udp.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS, REALTIME_TOS);
            } else {
                sys::set_int(udp.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_TCLASS, REALTIME_TOS);
            }
            // As much as the system allows (net.core.wmem_max, rmem_max).
            sys::set_int(udp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, UDP_BUFFER);
            sys::set_int(udp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, UDP_BUFFER);
            Some(udp)
        }
        None => None,
    };
    let (resume, detached) = match ours(&peer) {
        Some((id, detached)) => (Some(id), detached),
        None => (None, false),
    };
    let mut hello = hello;
    if let Some(udp) = &udp {
        hello["udp"] = json!(udp.local_addr()?.port());
    }
    hello["resume"] = json!(resume);
    hello["detached"] = json!(detached);
    // What the path is: the side that made it knows (a TCP connection
    // accepted could be the LAN's or a Wi-Fi Direct group's or the PC's own
    // network's, all alike to the side taking it).
    hello["path"] = json!(kind);
    let mut bytes = Vec::new();
    let mut body = 0u64.to_be_bytes().to_vec();  // numbered 0: before the session's own
    body.extend_from_slice(hello.to_string().as_bytes());
    frame::encode(frame::SESSION, 0, &body, &mut bytes);
    conn.writer().write_all(&bytes)?;
    while conn.wants_write() {
        conn.write_tls(&mut Until(&mut stream, until))?;
    }
    let mut decoder = Decoder::default();
    let theirs = loop {
        if let Some((kind, _, body)) = read_frame(&mut conn, &mut Until(&mut stream, until), &mut decoder)? {
            if kind != frame::SESSION || body.len() < 8 {
                return Err(invalid("a frame before the hello"));
            }
            let message: Value = serde_json::from_slice(&body[8..]).map_err(|e| invalid(e.to_string()))?;
            if message["t"] != "hello" {
                return Err(invalid("no hello"));
            }
            break message;
        }
    };
    let port = theirs["udp"].as_u64().filter(|p| (1..=65535).contains(p));
    let udp_peer = match (stream.addresses(), port) {
        (Some((peer, _)), Some(port)) if udp.is_some() => Some(SocketAddr::new(peer, port as u16)),
        _ => None,
    };
    let udp = if udp_peer.is_some() { udp } else { None };
    let resumes = resume.filter(|r| theirs["resume"].as_str() == Some(r.as_str()));
    let forced = theirs["detached"].as_bool().unwrap_or(false);
    // Taken, a TCP path is what its dialer says (both sides authenticated by now).
    let kind = match theirs["path"].as_str() {
        Some(said @ ("p2p" | "ap")) if !dialer && kind == "lan" => said.to_owned(),
        _ => kind.to_owned(),
    };
    Ok(Established { conn, stream, decoder, peer, hello: theirs, udp, udp_peer, sealer, opener, dialer,
                     kind, id, resumes, forced, quic: None })
}

/// A connection made ready to be a path (id: the session's number for
/// it): non-blocking from now on, its UDP socket (if any) taken into the
/// path.
fn prepare(established: &mut Established, id: u32) -> io::Result<Arc<Path>> {
    established.stream.set_timeouts(None)?;
    established.stream.set_nonblocking(true)?;
    let udp = established.udp.take();
    let quic = established.quic.take();
    // A QUIC path's stream is a socket pair's end (quic.rs): QUIC first.
    let transport = if quic.is_some() { "quic" } else { established.stream.transport() };
    let budget = established.stream.prior().filter(|prior| *prior != 0).or_else(|| prior(&established.kind))
        .filter(|_| quic.is_none()).unwrap_or(0);
    Ok(Arc::new(Path { id, kind: established.kind.clone(), stream: established.stream.try_clone()?, udp,
                       udp_peer: Mutex::new(established.udp_peer), quic, transport,
                       started: Instant::now(), heard_us: AtomicU64::new(1),
                       proven_us: AtomicU64::new(0), rtt_us: AtomicU64::new(0), realtime_us: AtomicU64::new(0),
                       gone: AtomicBool::new(false), leaving: AtomicBool::new(false), ended: AtomicBool::new(false),
                       state: AtomicU8::new(PathState::Active as u8), budget: AtomicU64::new(budget),
                       drained: AtomicBool::new(false), prior: budget }))
}

/// Blocking, during the hellos: the next frame (None: read more).
fn read_frame(conn: &mut rustls::Connection, stream: &mut dyn Read, decoder: &mut Decoder)
              -> io::Result<Option<(u8, u16, Vec<u8>)>> {
    if let Some(frame) = decoder.next()? {
        return Ok(Some(frame));
    }
    let mut plain = [0u8; 16 << 10];
    match conn.reader().read(&mut plain) {
        Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(n) => {
            decoder.push(&plain[..n]);
            return Ok(None);
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    if conn.read_tls(stream)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    conn.process_new_packets().map_err(|e| invalid(e.to_string()))?;
    Ok(None)
}

impl Session {
    /// A new session on its first path.
    pub(crate) fn start(node: &Arc<NodeInner>, mut established: Established) -> io::Result<Arc<Session>> {
        let hello = std::mem::take(&mut established.hello);
        let session = Arc::new(Session {
            peer: established.peer.clone(),
            name: hello["name"].as_str().unwrap_or("").to_owned(),
            id: established.id.clone(),
            features: Mutex::new(hello["features"].clone()),
            node: Arc::downgrade(node),
            closed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            channels: Mutex::default(),
            draining: Mutex::default(),
            pending: Mutex::default(),
            // Ours odd if we connected, even if we listened: never the same.
            next_id: AtomicU16::new(if established.dialer { 1 } else { 2 }),
            flow: hello["flow"].as_u64() == Some(1),
            pace: Mutex::new(crate::pace::Pace::new(Instant::now())),
            stream_budget: std::sync::atomic::AtomicUsize::new(STREAM_BUDGET),
            outbox: Mutex::default(),
            room: Condvar::new(),
            reliable: Mutex::new(Reliable { next: 1, ..Default::default() }),
            wake: Wake::new()?,
            path: Mutex::default(),
            live: Mutex::default(),
            // Ours says it too (NodeInner::hello).
            multipath: hello["paths"].as_u64() == Some(1),
            arriving: Mutex::default(),
            joining: Mutex::default(),
            retiring: Mutex::default(),
            retiring_any: AtomicBool::new(false),
            next_path: AtomicU32::new(1),
            sealer: std::mem::replace(&mut established.sealer, Sealer::new(&[0; 32], 0)),
            opener: Mutex::new(std::mem::replace(&mut established.opener, Opener::new(&[0; 32], 0))),
            dropped: AtomicU64::new(0),
        });
        // Its first path ready before anyone sees the session.
        let path = prepare(&mut established, session.next_path.fetch_add(1, Ordering::Relaxed))?;
        *session.path.lock().unwrap() = Some(path.clone());
        *session.live.lock().unwrap() = vec![path.clone()];
        let me = session.clone();
        std::thread::Builder::new().name("link-session".into()).spawn(move || me.run(established, path))?;
        Ok(session)
    }

    /// Another path for this session (both hellos named it): it takes over
    /// if it is better than the one now, if there is none, or if the other
    /// side has none (the one we believe in is dead for them); else closed.
    /// The session over, closed: looked at again under the lock end() lets
    /// go of those coming under (over between the two, end() had let go of
    /// them before this one came: it stayed, its stream open, until the
    /// session was dropped; on a cable, plus-one-link waited that long for
    /// the accessory).
    pub(crate) fn adopt(&self, established: Established) -> bool {
        if self.is_closed() {
            established.close();
            return false;
        }
        if self.multipath {
            return self.join(established);
        }
        let now = self.path.lock().unwrap().clone();
        // Taken if better: a higher rank, or QUIC over TCP at the same one.
        let better = |path: &Path| (rank(&established.kind), established.quic.is_some()) > (rank(&path.kind), path.quic.is_some());
        if !established.forced && now.is_some_and(|path| !path.gone.load(Ordering::SeqCst) && !better(&path)) {
            established.close();
            return false;
        }
        let mut arriving = self.arriving.lock().unwrap();
        if self.is_closed() {
            drop(arriving);
            established.close();
            return false;
        }
        if let Some(before) = arriving.replace(established) {
            before.close();
        }
        drop(arriving);
        self.wake.wake();
        true
    }

    /// With a peer that keeps several paths: taken (a reserve, or the
    /// carrier's place: the session's thread, join_all()) if the session
    /// has none of its kind (and fewer than MAX_PATHS, or a reserve worse
    /// than it), if it is better than the one of its kind (QUIC over TCP)
    /// or that one is suspect, or if the other side has no path; else
    /// closed, as before.
    fn join(&self, established: Established) -> bool {
        let refused = !established.forced && {
            let carrier = self.current();
            let live = self.live.lock().unwrap();
            let staying: Vec<&Arc<Path>> =
                live.iter().filter(|p| !p.gone.load(Ordering::SeqCst) && !p.leaving.load(Ordering::SeqCst)).collect();
            let theirs = (rank(&established.kind), established.quic.is_some());
            match staying.iter().find(|p| p.kind == established.kind) {
                Some(same) => same.state() != PathState::Suspect && theirs <= same.rank(),
                None => staying.len() >= MAX_PATHS && staying.iter()
                    .filter(|p| carrier.as_ref().is_none_or(|carrier| !Arc::ptr_eq(carrier, p)))
                    .all(|p| p.rank() >= theirs),
            }
        };
        if refused {
            established.close_notified();
            return false;
        }
        let mut joining = self.joining.lock().unwrap();
        if self.is_closed() {
            drop(joining);
            established.close();
            return false;
        }
        joining.push(established);
        drop(joining);
        self.wake.wake();
        true
    }

    /// What the other side serves now: {feature: version}.
    pub fn features(&self) -> Value {
        self.features.lock().unwrap().clone()
    }

    /// Ours changed (a feature served or no longer): the other side told.
    pub(crate) fn our_features(&self, features: Value) {
        self.send_message(json!({"t": "features", "features": features}));
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn current(&self) -> Option<Arc<Path>> {
        self.path.lock().unwrap().clone()
    }

    /// What it runs on now: "lan", "usb"; None while it waits for a path.
    pub fn path(&self) -> Option<String> {
        self.current().map(|path| path.kind.clone())
    }

    /// The path is a QUIC connection (quic.rs), not a TCP one or a stream.
    pub fn by_quic(&self) -> bool {
        self.current().is_some_and(|path| path.quic.is_some())
    }

    /// What carries the path now: "quic", "tcp" (the LAN's, a Wi-Fi Direct
    /// group's, the relay's), "stream" (a cable's or Bluetooth's: no IP);
    /// None while it waits for one. A TCP path moved to QUIC at the same
    /// kind is a path taken like any other: Event::Moved says it.
    pub fn transport(&self) -> Option<&'static str> {
        self.current().map(|path| path.transport)
    }

    /// What the path's congestion controller allows now (bytes a second):
    /// a QUIC path's (cc.rs), a Bluetooth path's or a cable's what its
    /// stream drains (Drain); None on a path with none of its own (TCP).
    pub fn rate(&self) -> Option<u64> {
        self.current().and_then(|path| path.budget().0)
    }

    /// The last round trip over UDP, if it works.
    pub fn rtt(&self) -> Option<Duration> {
        self.current().filter(|path| path.udp_works()).map(|path| Duration::from_micros(path.rtt_us.load(Ordering::Relaxed)))
    }

    /// Its paths as they are now: the one it runs on first, then its
    /// reserves (none while it waits for one).
    pub fn paths(&self) -> Vec<PathInfo> {
        self.live.lock().unwrap().iter().map(|path| path.info()).collect()
    }

    /// Bytes sent and not yet on the air (the UDP socket's, the queue's,
    /// the Wi-Fi driver's: each packet counts until it is transmitted; on
    /// QUIC, the datagrams quinn holds for the window) and the stream's
    /// backlog: how far behind the network is. (Without quinn's, the
    /// desktop never saw the realtime wait on QUIC: never a frame left out
    /// before it was encoded, and then packets dropped late, frames broken.)
    pub fn queued(&self) -> usize {
        let path = self.current();
        let mut queued: libc::c_int = 0;
        if let Some(udp) = path.as_ref().and_then(|path| path.udp.as_ref()) {
            unsafe { libc::ioctl(udp.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
        }
        let quic = path.as_ref().and_then(|path| path.quic.as_ref()).map_or(0, |quic| quic.queued());
        queued.max(0) as usize + quic + self.outbox.lock().unwrap().len()
    }

    /// The channels open now.
    pub fn channels(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    /// Input and realtime messages dropped (no room on the way, or no path).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The session over, for every channel at once; the other side told
    /// (a moment for "bye" to go, if there is a path: else it hears
    /// nothing, and gives up after RESUME_WAIT).
    pub fn close(&self) {
        if self.is_closed() || self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.current().is_some() {
            self.send_message(json!({"t": "bye"}));
            let until = Instant::now() + Duration::from_millis(300);
            while !self.outbox.lock().unwrap().is_empty() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(20));  // from rustls to the socket
        }
        self.end("closed here");
    }

    fn end(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        log!("{}: the session is over ({reason})", self.label());
        // Let go of, not only shut: a handle on the session kept anywhere
        // (a feature's thread, an app's) keeps none of its sockets.
        if let Some(path) = self.path.lock().unwrap().take() {
            end_path(&path);
        }
        for path in self.live.lock().unwrap().drain(..) {
            end_path(&path);
        }
        if let Some(arriving) = self.arriving.lock().unwrap().take() {
            arriving.close();
        }
        for joining in self.joining.lock().unwrap().drain(..) {
            joining.close();
        }
        self.retire(None);
        self.wake.wake();
        for (_, waiting) in self.pending.lock().unwrap().drain() {
            let _ = waiting.send(Err("the session is over".into()));
        }
        for (_, end) in self.channels.lock().unwrap().drain() {
            // Its inbox let go too: its thread holds the end, and would
            // wait for it for ever (and keep its socket).
            end.inbox.lock().unwrap().take();
            sys::shutdown(end.fd.as_raw_fd());
            end.gone();
        }
        // Closed by the other side, their inbox and pump still there: they
        // wait on a feature that may neither read nor close (the pump holds
        // the session).
        for end in self.draining.lock().unwrap().drain(..).filter_map(|end| end.upgrade()) {
            sys::shutdown(end.fd.as_raw_fd());
        }
        // Under the outbox's lock: a pump looks at closed under it, then waits.
        drop(self.outbox.lock().unwrap());
        self.room.notify_all();
        if let Some(node) = self.node.upgrade() {
            node.gone(self, reason);
        }
    }

    fn label(&self) -> String {
        if self.name.is_empty() { self.peer[..8].to_owned() } else { self.name.clone() }
    }

    // -- the channels ----------------------------------------------------------------

    /// A channel of ours, once the other side takes it.
    pub fn open(self: &Arc<Self>, feature: &str, name: &str, class: Class) -> Result<Channel, String> {
        if self.is_closed() {
            return Err("the session is over".into());
        }
        let (ours, theirs) = sys::seqpacket_pair(CHANNEL_BUFFER).map_err(|e| e.to_string())?;
        // Ours before the other side hears of it: what it sends at once waits in the socket.
        let end = {
            let mut channels = self.channels.lock().unwrap();
            if channels.len() >= MAX_CHANNELS {
                return Err(format!("{feature}/{name}: {MAX_CHANNELS} channels open already"));
            }
            // Ids come round again (u16): none in use, and never the path's own.
            let id = loop {
                let id = self.next_id.fetch_add(2, Ordering::Relaxed);
                if id != DATAGRAM_PATH && !channels.contains_key(&id) {
                    break id;
                }
            };
            let end = Arc::new(End::new(id, class, ours));
            channels.insert(id, end.clone());
            end
        };
        let id = end.id;
        self.inbox(&end);
        let (answer, answered) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, answer);
        self.send_message(json!({"t": "open", "id": id, "feature": feature, "channel": name, "class": class.name()}));
        let result = answered.recv_timeout(OPEN_WAIT);
        self.pending.lock().unwrap().remove(&id);
        match result {
            Ok(Ok(())) => {
                self.pump(end);
                Ok(Channel { fd: theirs, id, class, feature: feature.into(), name: name.into() })
            }
            // Refused (or the session over): the other side has none.
            Ok(Err(e)) => {
                self.close_channel(id, false);
                Err(format!("{feature}/{name}: {e}"))
            }
            // No answer (the path lost meanwhile, say): the open may still
            // reach the other side, and the close goes after it.
            Err(_) => {
                self.close_channel(id, true);
                Err(format!("{feature}/{name}: no answer"))
            }
        }
    }

    /// The other side opens one: taken if a feature of ours serves it.
    fn incoming(self: &Arc<Self>, message: &Value) {
        let feature = message["feature"].as_str().unwrap_or("").to_owned();
        let name = message["channel"].as_str().unwrap_or("").to_owned();
        let handler = self.node.upgrade().and_then(|node| node.handler(&feature));
        // With its id as it came, whatever it is.
        let refuse = |reason: &str| self.send_message(json!({"t": "refuse", "id": message["id"], "reason": reason}));
        let Some(class) = message["class"].as_str().and_then(Class::parse) else { return refuse("unknown class") };
        let Some(handler) = handler else { return refuse("not served") };
        let Ok(id) = channel_id(message) else { return refuse("bad id") };
        if id == DATAGRAM_PATH || id % 2 == self.next_id.load(Ordering::Relaxed) % 2
            || self.channels.lock().unwrap().contains_key(&id) {
            return refuse("bad id");
        }
        if self.channels.lock().unwrap().len() >= MAX_CHANNELS {
            return refuse("too many channels");
        }
        let (ours, theirs) = match sys::seqpacket_pair(CHANNEL_BUFFER) {
            Ok(pair) => pair,
            Err(e) => return refuse(&e.to_string()),
        };
        let end = Arc::new(End::new(id, class, ours));
        self.inbox(&end);
        self.channels.lock().unwrap().insert(id, end.clone());
        self.send_message(json!({"t": "accept", "id": id}));
        self.pump(end);
        let incoming = Incoming { session: self.clone(), channel: Channel { fd: theirs, id, class, feature, name } };
        let _ = std::thread::Builder::new().name("link-incoming".into()).spawn(move || handler(incoming));
    }

    /// What the feature writes into its end, to the other side, until it
    /// closes it (or the channel or the session ends).
    fn pump(self: &Arc<Self>, end: Arc<End>) {
        let (me, id) = (self.clone(), end.id);
        let spawned = std::thread::Builder::new().name("link-channel".into()).spawn(move || {
            // A control message numbered (8 bytes more) is still a frame.
            let mut buf = vec![0u8; if end.class == Class::Control { frame::MAX - 8 } else { 64 << 10 }];
            let mut sealed = Vec::with_capacity(2048);
            loop {
                let length = match sys::recv(end.fd.as_raw_fd(), &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if me.is_closed() {
                    break;
                }
                if length > buf.len() {
                    log!("{}: channel {} sent a message of {length} bytes: dropped", me.label(), end.id);
                    continue;
                }
                match end.class {
                    Class::Control => {
                        if me.flow && !me.wait_for_credit(&end, length as u64) {
                            break;
                        }
                        me.wait_for_room(end.id);
                        me.queue_reliable(frame::MESSAGE, end.id, &buf[..length])
                    }
                    _ => me.send_datagram(end.id, &buf[..length], &mut sealed),
                }
            }
            me.close_channel(end.id, true);
        });
        if spawned.is_err() {
            self.close_channel(id, true);
        }
    }

    /// Until the other side's window has room for length more bytes on
    /// end (then counted as sent); false if the channel or the session is
    /// over meanwhile. Asleep until a credit comes or the channel goes.
    fn wait_for_credit(&self, end: &End, length: u64) -> bool {
        let mut granted = end.granted.lock().unwrap();
        loop {
            if self.is_closed() || end.gone.load(Ordering::Relaxed) {
                return false;
            }
            let sent = end.sent.load(Ordering::Relaxed);
            if sent.saturating_add(length) <= granted.saturating_add(WINDOW) {
                end.sent.store(sent + length, Ordering::Relaxed);
                return true;
            }
            granted = end.credit.wait(granted).unwrap();
        }
    }

    /// Flow: a control channel's inbox, emptied into its socket on a thread
    /// of its own (blocking there, not the session), telling the other side
    /// what its feature took each GRANT_EVERY.
    fn inbox(self: &Arc<Self>, end: &Arc<End>) {
        if !self.flow || end.class != Class::Control {
            return;
        }
        let (inbox, arriving) = mpsc::channel::<Option<Vec<u8>>>();
        *end.inbox.lock().unwrap() = Some(inbox);
        let (me, end) = (Arc::downgrade(self), end.clone());
        let _ = std::thread::Builder::new().name("link-inbox".into()).spawn(move || {
            let mut told = 0u64;
            while let Ok(Some(message)) = arriving.recv() {
                if sys::send(end.fd.as_raw_fd(), &message, true).is_err() {
                    break;  // its feature gone
                }
                let taken = end.taken.fetch_add(message.len() as u64, Ordering::Relaxed) + message.len() as u64;
                if taken - told >= GRANT_EVERY {
                    told = taken;
                    let Some(me) = me.upgrade() else { break };
                    me.send_message(json!({"t": "credit", "id": end.id, "taken": taken}));
                }
            }
            sys::shutdown(end.fd.as_raw_fd());
        });
    }

    /// Until the channel's own queue is under CONTROL_BACKLOG, or the
    /// session is over (the channels go in turn: one's backlog holds up no
    /// other). Asleep until the session's thread takes it under (room).
    fn wait_for_room(&self, id: u16) {
        let mut outbox = self.outbox.lock().unwrap();
        while !self.is_closed() && outbox.channel_len(id) > CONTROL_BACKLOG {
            outbox = self.room.wait(outbox).unwrap();
        }
    }

    fn close_channel(&self, id: u16, tell: bool) {
        let Some(end) = self.channels.lock().unwrap().remove(&id) else { return };
        end.gone();
        match end.inbox.lock().unwrap().take() {
            // Closed by the other side: what it sent before goes to the feature first.
            Some(inbox) if !tell => {
                let _ = inbox.send(None);
                let mut draining = self.draining.lock().unwrap();
                draining.retain(|end| end.strong_count() > 0);
                draining.push(Arc::downgrade(&end));
                drop(draining);
                if self.is_closed() {
                    sys::shutdown(end.fd.as_raw_fd());  // end() went by before it was there
                }
            }
            _ => sys::shutdown(end.fd.as_raw_fd()),
        }
        if tell && !self.is_closed() {
            self.send_message(json!({"t": "close", "id": id}));
        }
    }

    /// A message for a channel, from the other side, into its socket: a
    /// reliable one for a control channel, a datagram for the others
    /// (count: its counter, if it came sealed), else dropped.
    fn deliver(&self, id: u16, body: &[u8], reliable: bool, count: Option<u64>) {
        let Some(end) = self.channels.lock().unwrap().get(&id).cloned() else { return };
        if (end.class == Class::Control) != reliable {
            return;
        }
        if let (Class::Input, Some(count)) = (end.class, count) {
            // An older state must not undo a newer one.
            if count < end.newest.fetch_max(count.saturating_add(1), Ordering::Relaxed) {
                return;
            }
        }
        let overflowed = match end.inbox.lock().unwrap().as_ref() {
            Some(inbox) => {
                // The other side sends WINDOW past what it was told the
                // feature took, at most: more, and it ignores the flow.
                let received = end.received.fetch_add(body.len() as u64, Ordering::Relaxed) + body.len() as u64;
                let over = received > end.taken.load(Ordering::Relaxed).saturating_add(WINDOW + GRANT_EVERY);
                if !over {
                    let _ = inbox.send(Some(body.to_vec()));
                }
                Some(over)
            }
            None => None,
        };
        match overflowed {
            Some(true) => {
                log!("{}: channel {id} sent past its window: closed", self.label());
                self.close_channel(id, true);
                return;
            }
            Some(false) => return,
            None => {}
        }
        let fd = end.fd.as_raw_fd();
        let mut result = sys::send(fd, body, false);
        if end.class == Class::Control && result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
            && sys::writable(fd, CONTROL_STALL).unwrap_or(false) {
            result = sys::send(fd, body, false);
        }
        match result {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && end.class != Class::Control => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // Its feature gone, or not reading at all (a control message has no room).
                log!("{}: channel {id}: {e}", self.label());
                self.close_channel(id, true);
            }
        }
    }

    // -- the queue -------------------------------------------------------------------

    fn send_message(&self, message: Value) {
        let queue = match message["t"].as_str() {
            // After the channel's own messages, not ahead of them.
            Some("close") => Queue::Channel(message["id"].as_u64().unwrap_or(0) as u16),
            Some("bye") => Queue::Last,
            _ => Queue::Ahead,
        };
        self.outbox.lock().unwrap().push(Waiting { kind: frame::SESSION, channel: 0, body: message.to_string().into_bytes() },
                                         queue);
        self.wake.wake();
    }

    /// A channel's message, queued (numbered as it leaves).
    fn queue_reliable(&self, kind: u8, channel: u16, body: &[u8]) {
        self.outbox.lock().unwrap().push(Waiting { kind, channel, body: body.to_vec() }, Queue::Channel(channel));
        self.wake.wake();
    }

    /// Frames leaving: numbered in the order they leave, kept until the
    /// other side says it has them, onto out.
    fn number(&self, fresh: Vec<Waiting>, out: &mut Vec<u8>) {
        let mut reliable = self.reliable.lock().unwrap();
        for waiting in fresh {
            let number = reliable.next;
            reliable.next += 1;
            let mut numbered = number.to_be_bytes().to_vec();
            numbered.extend_from_slice(&waiting.body);
            let mut bytes = Vec::with_capacity(frame::HEADER + numbered.len());
            frame::encode(waiting.kind, waiting.channel, &numbered, &mut bytes);
            out.extend_from_slice(&bytes);
            reliable.unacked_bytes += bytes.len();
            reliable.unacked.push_back((number, bytes));
        }
    }

    fn queue(&self, kind: u8, channel: u16, body: &[u8]) {
        {
            let mut outbox = self.outbox.lock().unwrap();
            if kind == frame::DATAGRAM && outbox.urgent.len() > self.stream_budget.load(Ordering::Relaxed) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            frame::encode(kind, channel, body, &mut outbox.urgent);
        }
        self.wake.wake();
    }

    // -- the session's thread --------------------------------------------------------

    /// Path after path, until the session is over (with a peer that keeps
    /// several, the carrier and its reserves at once): each round over the
    /// paths there are; none left, another waited for.
    fn run(self: Arc<Self>, first: Established, path: Arc<Path>) {
        let mut paths = Paths { lives: Vec::new(), resumed: false, last: None, held: HashMap::new() };
        let mut plain = vec![0u8; 64 << 10];
        let kind = first.kind.clone();
        // The path given, not kept here: its socket goes with it.
        match self.take_up(first, Some(path)) {
            Ok(live) => {
                paths.lives.push(live);
                self.carry(&mut paths, 0);
                self.greet(&mut paths, 0);
            }
            Err(reason) => {
                self.live.lock().unwrap().clear();
                paths.last = Some((kind, reason));
                paths.resumed = true;
            }
        }
        loop {
            if self.is_closed() {
                break;
            }
            // Closing: the bye goes on the paths there are; close() ends
            // those that come.
            if !self.closing.load(Ordering::SeqCst) {
                if self.multipath {
                    self.join_all(&mut paths);
                } else {
                    self.arrive(&mut paths);
                }
            }
            if paths.lives.is_empty() {
                // Closing: close() ends it. Else another path, or the end.
                if self.closing.load(Ordering::SeqCst) || !self.away(&mut paths) {
                    break;
                }
                continue;
            }
            let until = if self.multipath { self.choose(&mut paths) } else { None };
            if paths.lives.is_empty() {
                continue;
            }
            let mut lost = self.round(&mut paths, until, &mut plain);
            // The last first: the indexes before stay as they were.
            lost.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
            lost.dedup_by_key(|(index, _)| *index);
            for (index, why) in lost {
                self.lose(&mut paths, index, why);
            }
        }
        // Over (or closing): every path ended and let go of (taken after
        // end() let go of those before).
        self.path.lock().unwrap().take();
        self.live.lock().unwrap().clear();
        for live in paths.lives.drain(..) {
            live.path.gone.store(true, Ordering::SeqCst);
            end_path(&live.path);
        }
        self.retire(None);
    }

    /// A connection made a path the session's thread drives (path: made
    /// already, the first): its datagrams read from now on (a QUIC path's
    /// budget told while it carries). What came with its hello is taken
    /// once it is the carrier or a reserve (greet()).
    fn take_up(self: &Arc<Self>, mut established: Established, path: Option<Arc<Path>>) -> Result<Live, String> {
        let path = match path {
            Some(path) => path,
            None => prepare(&mut established, self.next_path.fetch_add(1, Ordering::Relaxed)).map_err(|e| e.to_string())?,
        };
        let Established { mut conn, stream, decoder, .. } = established;
        if path.udp.is_some() {
            let (me, path) = (self.clone(), path.clone());
            let _ = std::thread::Builder::new().name("link-datagrams".into()).spawn(move || me.run_datagrams(path));
        }
        if let Some(quic) = &path.quic {
            let (me, here) = (self.clone(), path.clone());
            quic.read_datagrams(move |datagram| {
                let mut buf = datagram.to_vec();
                me.datagram_in(&here, &mut buf, None, &mut Vec::new());
            });
            // Its budget told as it moves, until the path goes (quic.close();
            // not after a better one took over: retire()), while it carries
            // (a reserve's controller is its own: the apps follow the
            // carrier's).
            let (me, budget, here) = (Arc::downgrade(self), quic.budget(), path.clone());
            let _ = std::thread::Builder::new().name("link-budget".into()).spawn(move || {
                let mut told = 0;
                while let Some((count, rate)) = budget.wait(told) {
                    told = count;
                    if here.gone.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Some(me) = me.upgrade() else { return };
                    if me.multipath && !me.carries(&here) {
                        continue;
                    }
                    if let Some(node) = me.node.upgrade() {
                        node.budget(&me, rate);
                    }
                }
            });
        }
        conn.set_buffer_limit(None);
        let now = Instant::now();
        let prior = Some(path.budget.load(Ordering::Relaxed)).filter(|prior| *prior != 0);
        Ok(Live { path, conn, stream, decoder, drain: Drain::new(now, prior), next_ping: now, next_udp_ping: now,
                  suspect_since: None, steady_since: now, taken_at: now, blocked: false, took: false, leaving: None })
    }

    /// The frames that came with paths.lives[index]'s hello, taken (as the
    /// path's first round would); an error (they broke the protocol): the
    /// path lost.
    fn greet(self: &Arc<Self>, paths: &mut Paths, index: usize) {
        let live = &mut paths.lives[index];
        let greeted = (|| {
            while let Some((kind, channel, body)) = live.decoder.next().map_err(|e| e.to_string())? {
                self.heard_on(&live.path);
                self.frame(kind, channel, body)?;
            }
            Ok::<(), String>(())
        })();
        if let Err(e) = greeted {
            self.lose(paths, index, Lost::Failed(e));
        }
    }

    /// Whether path is the one the session runs on now.
    fn carries(&self, path: &Path) -> bool {
        self.path.lock().unwrap().as_deref().is_some_and(|carrier| std::ptr::eq(carrier, path))
    }

    /// path out of Session::paths.
    fn unlive(&self, path: &Arc<Path>) {
        self.live.lock().unwrap().retain(|other| !Arc::ptr_eq(other, path));
    }

    /// paths.lives[index] the carrier from now on, first among them (the
    /// one before, if any, a reserve in its place): after another path,
    /// what that one had not delivered goes first again on it, the
    /// datagrams waiting for that one are dropped, and the move is told
    /// (Moved); its pace and its stream's measure from the start, its
    /// budget told as it begins (a stream's prior; with a peer that keeps
    /// several, a QUIC path's controller's now).
    fn carry(self: &Arc<Self>, paths: &mut Paths, index: usize) {
        paths.lives.swap(0, index);
        let now = Instant::now();
        if paths.resumed {
            let reliable = self.reliable.lock().unwrap();
            self.outbox.lock().unwrap().restart(reliable.unacked.iter().map(|(_, bytes)| bytes.clone()));
        }
        let carrier = &mut paths.lives[0];
        carrier.leaving = None;
        carrier.path.leaving.store(false, Ordering::SeqCst);
        *self.path.lock().unwrap() = Some(carrier.path.clone());
        {
            let mut live = self.live.lock().unwrap();
            live.retain(|other| !Arc::ptr_eq(other, &carrier.path));
            live.insert(0, carrier.path.clone());
        }
        *self.pace.lock().unwrap() = crate::pace::Pace::new(now);
        let drain = Drain::new(now, Some(carrier.path.budget.load(Ordering::Relaxed)).filter(|prior| *prior != 0));
        carrier.drain = if probes(&carrier.path.kind) { drain.probing(carrier.path.prior) } else { drain };
        (carrier.next_ping, carrier.next_udp_ping, carrier.taken_at) = (now, now, now);
        self.stream_budget.store(carrier.drain.wait_most(), Ordering::Relaxed);
        let budget = carrier.drain.budget.or_else(|| {
            carrier.path.quic.as_ref().map(|quic| quic.rate()).filter(|rate| self.multipath && *rate > 0)
        });
        if paths.resumed {
            if let Some(node) = self.node.upgrade() {
                node.moved(self);
            }
        }
        let state = |live: &Live, carries| match (live.suspect_since, carries) {
            (Some(_), _) => PathState::Suspect,
            (None, true) => PathState::Active,
            (None, false) => PathState::Standby,
        };
        self.tell_path(&paths.lives[0].path, state(&paths.lives[0], true));
        if index != 0 {
            self.tell_path(&paths.lives[index].path, state(&paths.lives[index], false));
        }
        if let (Some(rate), Some(node)) = (budget, self.node.upgrade()) {
            node.budget(self, rate);
        }
        paths.resumed = true;
    }

    /// paths.lives[index] lost (or let go of: Lost): out of the session's
    /// paths, ended, told (failed; closed, with a peer that keeps several,
    /// if either side let go of it on purpose); if it carried, the best
    /// reserve takes its place at once.
    fn lose(self: &Arc<Self>, paths: &mut Paths, index: usize, lost: Lost) {
        let live = paths.lives.remove(index);
        let path = &live.path;
        path.gone.store(true, Ordering::SeqCst);
        self.unlive(path);
        let reason = lost.reason();
        let ending = self.is_closed() || self.closing.load(Ordering::SeqCst);
        if !ending {
            if self.multipath && (live.leaving.is_some() || !matches!(lost, Lost::Failed(_))) {
                log!("{}: the {} path ({}) is let go of ({reason})", self.label(), path.kind, path.id);
                self.tell_path(path, PathState::Closed);
            } else {
                let silent = Duration::from_micros(path.silent());
                let suspect = match path.state() {
                    PathState::Suspect => format!(", suspect for {:.1} s", silent.saturating_sub(PATH_SUSPECT).as_secs_f64()),
                    _ => String::new(),
                };
                log!("{}: the {} path ({}) failed ({reason}), {:.1} s after the last heard{suspect}", self.label(), path.kind,
                     path.id, silent.as_secs_f64());
                self.tell_path(path, PathState::Failed);
                if self.multipath {
                    paths.hold(&path.kind, Instant::now());
                }
            }
        }
        if !self.multipath {
            self.retire(None);
        }
        end_path(path);
        if index != 0 {
            return;
        }
        paths.last = Some((path.kind.clone(), reason));
        // Closing too: the bye goes after what the lost one had not delivered.
        if self.multipath && !self.is_closed() {
            if let Some(best) = self.best_reserve(paths, true) {
                log!("{}: from {} to {} at once, the {} path lost", self.label(), path.kind, paths.lives[best].path.kind,
                     path.kind);
                paths.last = None;
                self.carry(paths, best);
            }
        }
    }

    /// paths.lives[index] let go of on purpose: TLS's close_notify first
    /// (not waiting for room), the other side takes its end for a path let
    /// go of, not one failed.
    fn let_go(self: &Arc<Self>, paths: &mut Paths, index: usize, why: &'static str) {
        let live = &mut paths.lives[index];
        live.conn.send_close_notify();
        while live.conn.wants_write() && live.conn.write_tls(&mut live.stream).is_ok_and(|n| n > 0) {}
        self.lose(paths, index, Lost::LetGo(why));
    }

    /// The reserve that takes the carrier's place: the best heard lately
    /// (rank()), one held (FLAP_HOLD) only if no other is; the carrier
    /// suspect, only one heard within RESERVE_HEARD (the other side stalled
    /// a moment, every path is about as silent: none taken, its next ping
    /// says); the carrier lost, any left rather than none (one suspect, one
    /// being let go of).
    fn best_reserve(&self, paths: &Paths, carrier_lost: bool) -> Option<usize> {
        paths.lives.iter().enumerate().skip(if carrier_lost { 0 } else { 1 })
            .filter(|(_, live)| carrier_lost || (live.leaving.is_none() && live.suspect_since.is_none()
                                                 && live.path.silent() < RESERVE_HEARD.as_micros() as u64))
            .max_by_key(|(_, live)| (live.leaving.is_none(), live.suspect_since.is_none(), !paths.held(&live.path.kind),
                                     live.path.rank()))
            .map(|(index, _)| index)
    }

    /// With a peer from before: the path adopt() took (better than the one
    /// now, the only one, or the other side's only one) takes over, the one
    /// now retired (retire()).
    fn arrive(self: &Arc<Self>, paths: &mut Paths) {
        let Some(established) = self.arriving.lock().unwrap().take() else { return };
        let to = established.kind.clone();
        if !paths.lives.is_empty() {
            let old = paths.lives.remove(0);
            log!("{}: from {} to {to}", self.label(), old.path.kind);
            old.path.gone.store(true, Ordering::SeqCst);
            self.unlive(&old.path);
            // Not ended: its datagrams still read, its stream (the path's
            // own clone of it) open until the other side is heard on the
            // new one.
            self.retire(Some(old.path.clone()));
        } else if let Some((kind, _)) = paths.last.take() {
            log!("{}: from {kind} to {to}", self.label());
        }
        match self.take_up(established, None) {
            Ok(live) => {
                self.live.lock().unwrap().push(live.path.clone());
                paths.lives.push(live);
                self.carry(paths, 0);
                self.greet(paths, 0);
            }
            Err(reason) => {
                self.retire(None);
                paths.last = Some((to, reason));
            }
        }
    }

    /// With a peer that keeps several paths: those adopt() took, each a
    /// reserve; the carrier's place if there is none, if the other side
    /// has no path (forced), or if the one of its kind it takes the place
    /// of carried (that one let go of once the other side is heard on this
    /// one, as a handover's: retire()); one too many, the worst reserve is
    /// let go of.
    fn join_all(self: &Arc<Self>, paths: &mut Paths) {
        let joining = std::mem::take(&mut *self.joining.lock().unwrap());
        for established in joining {
            let (kind, forced) = (established.kind.clone(), established.forced);
            let live = match self.take_up(established, None) {
                Ok(live) => live,
                Err(e) => {
                    log!("{}: a {kind} path not taken ({e})", self.label());
                    if paths.lives.is_empty() {
                        paths.last = Some((kind, e));
                    }
                    continue;
                }
            };
            let same = paths.lives.iter().position(|other| other.path.kind == kind && other.leaving.is_none());
            let alone = paths.lives.is_empty();
            self.live.lock().unwrap().push(live.path.clone());
            paths.lives.push(live);
            let new = paths.lives.len() - 1;
            if let Some(same) = same {
                let by = paths.lives[new].path.clone();
                paths.lives[same].path.leaving.store(true, Ordering::SeqCst);
                paths.lives[same].leaving = Some((by, Instant::now() + HANDOVER_CAP));
            }
            if alone || forced || same == Some(0) {
                match (alone, paths.last.take()) {
                    (true, Some((from, _))) => log!("{}: from {from} to {kind}", self.label()),
                    (true, None) => {}
                    (false, _) => log!("{}: from {} to {kind} at once ({})", self.label(), paths.lives[0].path.kind,
                                       if forced { "the other side has no path" } else { "a better one of its kind" }),
                }
                self.carry(paths, new);
                self.greet(paths, 0);
            } else {
                log!("{}: a {kind} path ({}) taken as a reserve", self.label(), paths.lives[new].path.id);
                self.tell_path(&paths.lives[new].path, PathState::Standby);
                self.greet(paths, new);
            }
            if paths.lives.iter().filter(|live| live.leaving.is_none()).count() > MAX_PATHS {
                let worst = paths.lives.iter().enumerate().skip(1).filter(|(_, live)| live.leaving.is_none())
                    .min_by_key(|(_, live)| live.path.rank()).map(|(index, _)| index);
                if let Some(worst) = worst {
                    self.let_go(paths, worst, "one path too many");
                }
            }
        }
    }

    /// With a peer that keeps several paths, before each round: one a
    /// better one of its kind took the place of is let go of once the
    /// other side is heard on that one (or HANDOVER_CAP after); the
    /// carrier suspect, the best reserve heard lately takes its place at
    /// once (the suspect's kind held: FLAP_HOLD); else a better reserve
    /// heard throughout HYSTERESIS does, its kind not held. When to look
    /// again, if something is due (the round's wait: no timer of its own).
    fn choose(self: &Arc<Self>, paths: &mut Paths) -> Option<Instant> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let soon = |at: Instant, next: &mut Option<Instant>| *next = Some(next.map_or(at, |next| next.min(at)));
        let mut index = 0;
        while index < paths.lives.len() {
            let live = &mut paths.lives[index];
            // The one that took its place gone already: it stays as it was.
            if live.leaving.as_ref().is_some_and(|(by, _)| by.gone.load(Ordering::SeqCst)) {
                live.leaving = None;
                live.path.leaving.store(false, Ordering::SeqCst);
            }
            let done = match &live.leaving {
                Some((by, until)) => {
                    let done = by.heard_us.load(Ordering::Relaxed) > 1 || now >= *until;
                    if !done {
                        soon(*until, &mut next);
                    }
                    done
                }
                None => false,
            };
            if done {
                self.let_go(paths, index, "a better one of its kind took its place");
            } else {
                index += 1;
            }
        }
        paths.held.retain(|_, at| now.duration_since(*at) < FLAP_HOLD);
        if paths.lives.len() < 2 {
            return next;
        }
        if paths.lives[0].suspect_since.is_some() {
            if let Some(best) = self.best_reserve(paths, false) {
                log!("{}: from {} to {} at once, the {} path suspect", self.label(), paths.lives[0].path.kind,
                     paths.lives[best].path.kind, paths.lives[0].path.kind);
                let suspect = paths.lives[0].path.kind.clone();
                paths.hold(&suspect, now);
                self.carry(paths, best);
            }
            return next;
        }
        let carrier = paths.lives[0].path.rank();
        let mut best: Option<usize> = None;
        for (index, live) in paths.lives.iter().enumerate().skip(1) {
            // Heard (the other side drives it too), and throughout HYSTERESIS.
            if live.leaving.is_some() || live.suspect_since.is_some() || live.path.heard_us.load(Ordering::Relaxed) <= 1
                || live.path.rank() <= carrier {
                continue;
            }
            let held = paths.held.get(&live.path.kind).map(|at| *at + FLAP_HOLD);
            let ready = held.map_or(live.steady_since + HYSTERESIS, |held| held.max(live.steady_since + HYSTERESIS));
            if now < ready {
                soon(ready, &mut next);
            } else if live.path.silent() < UPGRADE_HEARD.as_micros() as u64
                && best.is_none_or(|best| live.path.rank() > paths.lives[best].path.rank()) {
                best = Some(index);  // else at its next ping (it wakes the round)
            }
        }
        if let Some(best) = best {
            log!("{}: from {} to {}, a better path", self.label(), paths.lives[0].path.kind, paths.lives[best].path.kind);
            self.carry(paths, best);
        }
        next
    }

    /// No path left: told (Detached), and another waited for RESUME_WAIT
    /// (RESUME_WAIT_IN_USE with a channel open), asleep until one comes or
    /// the session ends; false if it is over.
    fn away(self: &Arc<Self>, paths: &mut Paths) -> bool {
        let (kind, reason) = paths.last.take().unwrap_or_default();
        log!("{}: the {kind} path is lost ({reason}); waiting for another", self.label());
        *self.path.lock().unwrap() = None;
        if let Some(node) = self.node.upgrade() {
            node.detached(self);
        }
        let wait = if self.channels() > 0 { RESUME_WAIT_IN_USE } else { RESUME_WAIT };
        let until = Instant::now() + wait;
        loop {
            if self.is_closed() {
                return false;
            }
            let came = if self.multipath {
                self.joining.lock().unwrap().first().map(|established| established.kind.clone())
            } else {
                self.arriving.lock().unwrap().as_ref().map(|established| established.kind.clone())
            };
            if let Some(kind) = came {
                log!("{}: on again, by {kind}", self.label());
                return true;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.end(&format!("no path for {} s", wait.as_secs()));
                return false;
            }
            // Woken by a path coming (adopt) or the session's end.
            let _ = sys::readable(self.wake.fd(), Some(left));
            self.wake.clear();
        }
    }

    /// path's state from now on (the session's thread alone moves it),
    /// told (Event::Path).
    fn tell_path(self: &Arc<Self>, path: &Path, state: PathState) {
        path.state.store(state as u8, Ordering::Relaxed);
        if let Some(node) = self.node.upgrade() {
            node.path(self, path.info());
        }
    }

    /// The path a handover left (None: none), and the one it left before,
    /// if still there, ended now. The other side takes the new path a
    /// moment after us (its hellos' end is behind ours) and until then would
    /// take the old one's end for a path lost: detached, then moved (the
    /// tablet's "Reconnecting…" over the desktop at each move to QUIC, the
    /// PC dialing again). So the old one ends once something of the other
    /// side's comes on the new one (heard_on: nothing comes on a path before
    /// its session takes it), or HANDOVER_CAP after (drive's own wait: no
    /// timer of its own). Meanwhile nobody reads its stream: what the other
    /// side still sends there comes again on the new one (not
    /// acknowledged); its stream is a clone of the session thread's (that
    /// one dropped: no FIN while this one is open); its datagrams still
    /// count.
    fn retire(&self, path: Option<Arc<Path>>) {
        let until = Instant::now() + HANDOVER_CAP;
        let mut retiring = self.retiring.lock().unwrap();
        self.retiring_any.store(path.is_some(), Ordering::SeqCst);
        let before = std::mem::replace(&mut *retiring, path.map(|path| (path, until)));
        drop(retiring);
        if let Some((before, _)) = before {
            end_path(&before);
        }
    }

    /// Something authenticated came on path (its stream's frame, a sealed
    /// datagram): if it is not the path a handover left, the other side is
    /// on the new one too, and the old one ends.
    fn heard_on(&self, path: &Path) {
        if self.retiring_any.load(Ordering::Relaxed) {
            self.end_retiring(|old, _| !std::ptr::eq(old, path));
        }
    }

    /// When the path a handover left ends at the latest, if there is one;
    /// ended now if that is past.
    fn retiring_until(&self, now: Instant) -> Option<Instant> {
        if !self.retiring_any.load(Ordering::Relaxed) {
            return None;
        }
        if self.end_retiring(|_, until| until <= now) {
            log!("{}: nothing heard on the new path in {} s: the one before ends", self.label(), HANDOVER_CAP.as_secs());
            return None;
        }
        self.retiring.lock().unwrap().as_ref().map(|(_, until)| *until)
    }

    /// The path a handover left ended if done(it, until) says so (under the
    /// lock: a handover meanwhile is not mistaken for it): whether it was.
    fn end_retiring(&self, done: impl FnOnce(&Path, Instant) -> bool) -> bool {
        let mut retiring = self.retiring.lock().unwrap();
        let Some((old, _)) = retiring.take_if(|(old, until)| done(&**old, *until)) else { return false };
        self.retiring_any.store(false, Ordering::SeqCst);
        drop(retiring);
        end_path(&old);
        true
    }

    /// One round of the session's thread over its paths, the carrier
    /// first: what was queued goes out on the carrier, once the last of it
    /// left (the urgent first, then a slice of the reliable:
    /// Outbox::take); each path's TLS out to its socket; the carrier's
    /// stream measured (Drain); each path's silence looked at; the pings
    /// (a reserve's, each PING_EVERY, nothing else); a wait for what comes
    /// on any of them or for the next thing due (until, at the latest);
    /// what came taken. The paths lost meanwhile, and why.
    fn round(self: &Arc<Self>, paths: &mut Paths, until: Option<Instant>, plain: &mut [u8]) -> Vec<(usize, Lost)> {
        let Paths { lives, held, .. } = paths;
        let mut lost = Vec::new();
        let mut paced = None;
        if !lives[0].conn.wants_write() {
            let reliable = {
                let mut pace = self.pace.lock().unwrap();
                let room = self.reliable.lock().unwrap().unacked_bytes < UNACKED_MOST;
                if room && pace.may_send(Instant::now()) { WRITE_SLICE } else { 0 }
            };
            let (mut out, fresh, taken, made_room) = {
                let mut outbox = self.outbox.lock().unwrap();
                let (out, fresh, taken) = outbox.take(reliable);
                (out, fresh, taken, std::mem::take(&mut outbox.made_room))
            };
            if made_room {
                self.room.notify_all();
            }
            if !fresh.is_empty() {
                self.number(fresh, &mut out);
            }
            let mut pace = self.pace.lock().unwrap();
            pace.spent(taken);
            paced = pace.wait();
            if !out.is_empty() {
                if let Err(e) = lives[0].conn.writer().write_all(&out) {
                    lost.push((0, Lost::Failed(e.to_string())));
                    return lost;
                }
            }
        }
        for (index, live) in lives.iter_mut().enumerate() {
            (live.blocked, live.took) = (false, false);
            while live.conn.wants_write() {
                match live.conn.write_tls(&mut live.stream) {
                    Ok(written) => {
                        if index == 0 {
                            live.drain.wrote(written);
                        }
                        live.took |= written > 0;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        live.blocked = true;
                        break;
                    }
                    Err(e) => {
                        // A stream the other side let go of (EPIPE,
                        // ECONNRESET): what it sent before is taken first,
                        // a "bye" there ending the session (lost, the
                        // session waited for another path).
                        let _ = self.take_in(live, plain);
                        lost.push((index, Lost::Failed(e.to_string())));
                        break;
                    }
                }
            }
        }
        if !lost.is_empty() {
            return lost;
        }
        // All out, and more queued meanwhile (its wake taken already, while
        // the stream was full): round again at once, after a look at what
        // came, not at the next ping (a transfer went a burst a second).
        // Not for reliable frames that may not go: paced, or as much
        // unacknowledged as may be (the acknowledgement comes on the
        // stream, and wakes the poll).
        let carrier = &mut lives[0];
        let room = self.reliable.lock().unwrap().unacked_bytes < UNACKED_MOST;
        let more = !carrier.blocked && {
            let outbox = self.outbox.lock().unwrap();
            !outbox.urgent.is_empty() || (outbox.reliable_bytes > 0 && paced.is_none() && room)
        };
        carrier.drain.after(carrier.blocked, !carrier.blocked && !more);
        if let Some((wait, told)) = carrier.drain.roll(Instant::now()) {
            self.stream_budget.store(wait, Ordering::Relaxed);
            carrier.path.budget.store(carrier.drain.budget.unwrap_or(0), Ordering::Relaxed);
            carrier.path.drained.store(carrier.drain.measured, Ordering::Relaxed);
            if let (Some(rate), Some(node)) = (told, self.node.upgrade()) {
                node.budget(self, rate);
            }
            if let Some(every) = carrier.drain.probe_note(Instant::now()) {
                let why = if every > PROBE_AFTER { "measured back down each time" } else { "the last time held" };
                log!("{}: the {} path's budget goes up each {} s at most ({why})", self.label(), carrier.path.kind,
                     every.as_secs());
            }
        }
        let now = Instant::now();
        for (index, live) in lives.iter_mut().enumerate() {
            let silent = live.path.silent();
            if silent > PATH_SILENCE.as_micros() as u64 {
                lost.push((index, Lost::Failed(format!("nothing heard for {:.1} s", silent as f64 / 1e6))));
                continue;
            }
            // Heard (the other side sends) and taking nothing of what
            // waits: lost as well (its answers, our pings, piled up for ever).
            if live.took || !live.blocked {
                live.taken_at = now;
            } else if now.duration_since(live.taken_at) > PATH_SILENCE {
                lost.push((index, Lost::Failed(format!("nothing taken for {:.1} s",
                                                       now.duration_since(live.taken_at).as_secs_f64()))));
                continue;
            }
            // Nothing heard for PATH_SUSPECT, though the other side pings
            // each PING_EVERY: suspect, until something comes (a datagram
            // on a suspect path wakes this).
            let suspect = silent >= PATH_SUSPECT.as_micros() as u64;
            if suspect != live.suspect_since.is_some() {
                if suspect {
                    log!("{}: the {} path ({}) is suspect: nothing heard for {:.1} s", self.label(), live.path.kind,
                         live.path.id, silent as f64 / 1e6);
                    live.suspect_since = Some(now);
                    // A reserve of a kind held, gone silent again: held on.
                    if index > 0 && held.get(&live.path.kind).is_some_and(|at| now.duration_since(*at) < FLAP_HOLD) {
                        held.insert(live.path.kind.clone(), now);
                    }
                } else if let Some(since) = live.suspect_since.take() {
                    log!("{}: the {} path ({}) heard again, suspect for {:.1} s", self.label(), live.path.kind,
                         live.path.id, now.duration_since(since).as_secs_f64());
                    live.steady_since = now;
                }
                let state = match (suspect, index) {
                    (true, _) => PathState::Suspect,
                    (false, 0) => PathState::Active,
                    (false, _) => PathState::Standby,
                };
                self.tell_path(&live.path, state);
            }
        }
        if !lost.is_empty() {
            return lost;
        }
        let ack_due = {
            let reliable = self.reliable.lock().unwrap();
            reliable.heard >= reliable.acknowledged.saturating_add(ACK_AFTER)
        };
        let mut pinged = false;
        if now >= lives[0].next_ping || ack_due {
            lives[0].next_ping = now + PING_EVERY;
            let heard = {
                let mut reliable = self.reliable.lock().unwrap();
                reliable.acknowledged = reliable.heard;
                reliable.heard
            };
            self.queue(frame::PING, 0, &heard.to_be_bytes());
            pinged = true;
        }
        // The reserves': each PING_EVERY on the stream and over the
        // datagrams at once, in step with the carrier's (one wake for
        // all), and nothing else; none on one being let go of.
        let step = lives[0].next_ping;
        for (index, live) in lives.iter_mut().enumerate().skip(1) {
            if live.leaving.is_some() || now < live.next_ping {
                continue;
            }
            live.next_ping = if step >= now + PING_EVERY / 2 { step } else { now + PING_EVERY };
            let mut ping = Vec::new();
            frame::encode(frame::PING, 0, &self.reliable.lock().unwrap().heard.to_be_bytes(), &mut ping);
            if let Err(e) = live.conn.writer().write_all(&ping) {
                lost.push((index, Lost::Failed(e.to_string())));
                continue;
            }
            if live.path.udp.is_some() || live.path.quic.is_some() {
                let mut ping = vec![PING];
                ping.extend_from_slice(&live.path.us().to_be_bytes());
                self.send_datagram_now(&live.path, DATAGRAM_PATH, &ping, &mut Vec::new());
            }
            pinged = true;
        }
        if pinged || !lost.is_empty() {
            return lost;  // out at once: the next round
        }
        let carrier = &mut lives[0];
        let datagrams = carrier.path.udp.is_some() || carrier.path.quic.is_some();
        if datagrams && now >= carrier.next_udp_ping {
            let path = &carrier.path;
            let early = path.started.elapsed() < UDP_PROOF;
            let busy = path.realtime_going() && self.outbox.lock().unwrap().reliable_bytes > 0;
            carrier.next_udp_ping = now + if busy && path.udp_works() { PROBE_BUSY }
                else if path.udp_works() || !early { PING_EVERY } else { UDP_PROBE };
            let mut ping = vec![PING];
            ping.extend_from_slice(&path.us().to_be_bytes());
            self.send_datagram_now(path, DATAGRAM_PATH, &ping, &mut Vec::new());
        }
        let mut deadline = if datagrams { carrier.next_ping.min(carrier.next_udp_ping) } else { carrier.next_ping };
        for (index, live) in lives.iter().enumerate() {
            if index > 0 && live.leaving.is_none() {
                deadline = deadline.min(live.next_ping);
            }
            if live.suspect_since.is_none() {
                // When it is suspect, if nothing comes before: no timer of its own.
                deadline = deadline.min(now + PATH_SUSPECT.saturating_sub(Duration::from_micros(live.path.silent())));
            }
        }
        if let Some(until) = until {
            deadline = deadline.min(until);  // the caller's: a reserve ready, one let go of at the latest
        }
        if let Some(until) = self.retiring_until(now) {
            deadline = deadline.min(until);  // the path a handover left, at the latest
        }
        let mut wait = deadline.saturating_duration_since(now);
        if let Some(paced) = paced.filter(|_| self.outbox.lock().unwrap().reliable_bytes > 0) {
            wait = wait.min(paced);  // the reliable frames' next turn
        }
        let mut fds: Vec<libc::pollfd> = std::iter::once(libc::pollfd { fd: self.wake.fd(), events: libc::POLLIN, revents: 0 })
            .chain(lives.iter().map(|live| libc::pollfd {
                fd: live.stream.as_raw_fd(),
                events: libc::POLLIN | if live.blocked { libc::POLLOUT } else { 0 },
                revents: 0,
            }))
            .collect();
        if let Err(e) = sys::poll(&mut fds, Some(if more { Duration::ZERO } else { wait.min(PING_EVERY) })) {
            lost.push((0, Lost::Failed(e.to_string())));
            return lost;
        }
        if fds[0].revents != 0 {
            self.wake.clear();
        }
        for (index, live) in lives.iter_mut().enumerate() {
            if fds[index + 1].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
                continue;
            }
            match self.take_in(live, plain) {
                Ok(In::Open) => {}
                Ok(In::Ended) => lost.push((index, Lost::Failed("the other side closed".into()))),
                Ok(In::Notified) => lost.push((index, Lost::Notified)),
                Err(e) => lost.push((index, Lost::Failed(e))),
            }
        }
        lost
    }

    /// What came on a path's stream, until it would block: every whole
    /// frame taken, even when the stream ended or failed right after (a
    /// "bye" there ends the session, not only the path: lost, the session
    /// waited RESUME_WAIT for another, and the PC reached for the
    /// tablet). How the stream is: open still, at its end, or told it ends
    /// (TLS's close_notify: the other side let go of it on purpose).
    fn take_in(self: &Arc<Self>, live: &mut Live, plain: &mut [u8]) -> Result<In, String> {
        let read = 'read: loop {
            match live.conn.read_tls(&mut live.stream) {
                Ok(0) => break Ok(In::Ended),
                Ok(_) => {
                    if let Err(e) = live.conn.process_new_packets() {
                        break Err(e.to_string());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(In::Open),
                Err(e) => break Err(e.to_string()),
            }
            // Out at once: rustls holds only so much read and not taken.
            loop {
                match live.conn.reader().read(plain) {
                    Ok(0) => break 'read Ok(In::Notified),
                    Ok(n) => live.decoder.push(&plain[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => break 'read Err(e.to_string()),
                }
            }
        };
        while let Some((kind, channel, body)) = live.decoder.next().map_err(|e| e.to_string())? {
            live.path.heard();
            self.heard_on(&live.path);
            self.frame(kind, channel, body)?;
        }
        read
    }

    /// A frame from the stream; an error ends the path (the other side
    /// broke the protocol).
    fn frame(self: &Arc<Self>, kind: u8, channel: u16, body: Vec<u8>) -> Result<(), String> {
        match kind {
            frame::SESSION | frame::MESSAGE => {
                if body.len() < 8 {
                    return Err("a reliable frame with no number".into());
                }
                let number = u64::from_be_bytes(body[..8].try_into().unwrap());
                {
                    let mut reliable = self.reliable.lock().unwrap();
                    if number <= reliable.heard {
                        return Ok(());  // had it: sent again after a path was lost
                    }
                    // A stream keeps them in order, and none is skipped.
                    if number != reliable.heard + 1 {
                        return Err(format!("frame {number} after {}", reliable.heard));
                    }
                    reliable.heard = number;
                }
                if kind == frame::MESSAGE {
                    self.deliver(channel, &body[8..], true, None);
                } else {
                    match serde_json::from_slice::<Value>(&body[8..]) {
                        Ok(message) => self.message(&message),
                        Err(e) => log!("{}: a bad message: {e}", self.label()),
                    }
                    let ahead = self.outbox.lock().unwrap().ahead_bytes;
                    if ahead > AHEAD_MOST {
                        return Err(format!("{} KiB of answers waiting, none taken", ahead >> 10));
                    }
                }
            }
            frame::DATAGRAM => self.deliver(channel, &body, false, None),
            frame::PING if body.len() >= 8 => {
                let acknowledged = u64::from_be_bytes(body[..8].try_into().unwrap());
                let mut reliable = self.reliable.lock().unwrap();
                while reliable.unacked.front().is_some_and(|(number, _)| *number <= acknowledged) {
                    if let Some((_, bytes)) = reliable.unacked.pop_front() {
                        reliable.unacked_bytes -= bytes.len();
                    }
                }
            }
            _ => log!("{}: a frame of kind {kind}", self.label()),
        }
        Ok(())
    }

    fn message(self: &Arc<Self>, message: &Value) {
        let kind = message["t"].as_str().unwrap_or("");
        let id = match kind {
            "accept" | "refuse" | "close" | "credit" => match channel_id(message) {
                Ok(id) => id,
                Err(e) => return log!("{}: a {kind:?} for {e}: dropped", self.label()),
            },
            _ => DATAGRAM_PATH,  // none
        };
        match kind {
            "open" => self.incoming(message),
            "accept" | "refuse" => {
                if let Some(waiting) = self.pending.lock().unwrap().remove(&id) {
                    let result = if message["t"] == "accept" {
                        Ok(())
                    } else {
                        Err(message["reason"].as_str().unwrap_or("refused").to_owned())
                    };
                    let _ = waiting.send(result);
                }
            }
            "close" => self.close_channel(id, false),
            "bye" => self.end("closed by the other side"),
            "features" => *self.features.lock().unwrap() = message["features"].clone(),
            "credit" => {
                let end = self.channels.lock().unwrap().get(&id).cloned();
                if let (Some(end), Some(taken)) = (end, message["taken"].as_u64()) {
                    let mut granted = end.granted.lock().unwrap();
                    *granted = (*granted).max(taken);
                    end.credit.notify_all();
                }
            }
            other => log!("{}: a message of type {other:?}", self.label()),
        }
    }

    // -- the datagrams ---------------------------------------------------------------

    /// Input and realtime: a datagram once UDP is proven, else over the
    /// stream; no path: lost.
    fn send_datagram(&self, channel: u16, body: &[u8], sealed: &mut Vec<u8>) {
        if body.len() > MAX_DATAGRAM {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let current = self.current();
        if let Some(path) = &current {
            path.realtime_us.store(path.us().max(1), Ordering::Relaxed);
        }
        match current {
            Some(path) if path.udp_works() => self.send_datagram_now(&path, channel, body, sealed),
            Some(_) => self.queue(frame::DATAGRAM, channel, body),
            None => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn send_datagram_now(&self, path: &Path, channel: u16, body: &[u8], sealed: &mut Vec<u8>) {
        if let Some(quic) = &path.quic {
            self.sealer.seal(channel, body, sealed);
            if !quic.send(sealed) {
                self.dropped.fetch_add(1, Ordering::Relaxed);  // late, or too big
            }
            return;
        }
        let (Some(udp), Some(to)) = (&path.udp, *path.udp_peer.lock().unwrap()) else { return };
        self.sealer.seal(channel, body, sealed);
        let mut result = sys::send_to(udp.as_raw_fd(), sealed, &to);
        // No room: a moment for some (a keyframe's tail), then lost.
        if result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
            && sys::writable(udp.as_raw_fd(), UDP_STALL).unwrap_or(false) {
            result = sys::send_to(udp.as_raw_fd(), sealed, &to);
        }
        if result.is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn run_datagrams(self: Arc<Self>, path: Arc<Path>) {
        let Some(udp) = &path.udp else { return };
        let mut buf = vec![0u8; 2048];
        let mut sealed = Vec::with_capacity(64);
        // Until the path ends, not when it is left: a handover's old path is
        // read as its QUIC datagrams are (retire()). Asleep in the read
        // until a datagram comes or end_path shuts the socket down.
        while !self.is_closed() && !path.ended.load(Ordering::SeqCst) {
            let (length, from) = match udp.recv_from(&mut buf) {
                Ok(got) => got,
                Err(_) => continue,  // shut down (no address): look at ended again
            };
            self.datagram_in(&path, &mut buf[..length], Some(from), &mut sealed);
        }
    }

    /// A datagram that came on path (from: where, for UDP): its channel's,
    /// or a ping answered, or a pong's round trip taken.
    fn datagram_in(&self, path: &Path, buf: &mut [u8], from: Option<SocketAddr>, sealed: &mut Vec<u8>) {
        let Some((count, channel, body)) = self.opener.lock().unwrap().open(buf) else { return };
        path.heard();
        if path.state() == PathState::Suspect && !path.gone.load(Ordering::Relaxed) {
            self.wake.wake();  // active again: the session's thread says so (drive)
        }
        self.heard_on(path);
        // Where it came from is where the other side is now (its address
        // may change; only a sealed packet can say so).
        if let Some(from) = from {
            let mut peer = path.udp_peer.lock().unwrap();
            if *peer != Some(from) {
                *peer = Some(from);
            }
        }
        if channel != DATAGRAM_PATH {
            self.deliver(channel, body, false, Some(count));
        } else if body.len() == 9 && body[0] == PING {
            let mut pong = body.to_vec();
            pong[0] = PONG;
            self.send_datagram_now(path, DATAGRAM_PATH, &pong, sealed);
        } else if body.len() == 9 && body[0] == PONG {
            let sent = u64::from_be_bytes(body[1..9].try_into().unwrap());
            let now = path.us();
            path.rtt_us.store(now.saturating_sub(sent), Ordering::Relaxed);
            path.proven_us.store(now.max(1), Ordering::Relaxed);
            // A QUIC path is paced by its own controller (cc.rs); pace.rs
            // paces a TCP one, the carrier (a reserve's round trips are
            // not the carrier's).
            if path.quic.is_none() && (!self.multipath || self.carries(path)) {
                self.pace.lock().unwrap().sample(now.saturating_sub(sent), path.realtime_going(), Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! A PC's session with a tablet faked by hand: the frames a broken or
    //! hostile one may send, or not. The session's threads are counted by
    //! their names: one test at a time.

    use super::*;
    use crate::identity::{Identity, Paired};
    use crate::{Event, Node};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
    }

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

    /// The tablet's end of a session, by hand: its TLS, its stream, its
    /// reliable frames numbered here.
    struct Fake {
        conn: rustls::Connection,
        stream: Box<dyn Stream>,
        decoder: Decoder,
        /// Its UDP socket, if the stream has IP under it (open, never
        /// read), the PC's, and its datagrams' sealer.
        udp: Option<UdpSocket>,
        udp_peer: Option<SocketAddr>,
        sealer: Sealer,
        next: u64,
    }

    impl Fake {
        /// The TLS handshake and the hellos on the tablet's side of the
        /// connection stream makes, on a thread (the PC's side waits for
        /// them): flow controlled or not, a path for the session resumes
        /// names (None: a new one).
        fn start(tablet: &Identity, pc: &str, flow: bool, resumes: Option<String>,
                 stream: impl FnOnce() -> Box<dyn Stream> + Send + 'static) -> std::thread::JoinHandle<Fake> {
            let said = if flow { json!({"flow": 1}) } else { json!({}) };
            Fake::start_saying(tablet, pc, said, resumes.map(|id| (id, true)), stream)
        }

        /// As start(), its hello saying said's fields too, a path for the
        /// session resumes names (and whether the tablet has no path for
        /// it: the PC must take this one).
        fn start_saying(tablet: &Identity, pc: &str, said: Value, resumes: Option<(String, bool)>,
                        stream: impl FnOnce() -> Box<dyn Stream> + Send + 'static) -> std::thread::JoinHandle<Fake> {
            let server = tablet.server_config(trusting(pc)).unwrap();
            let mut hello = json!({"t": "hello", "name": "tablet", "features": {"sink": 1}});
            for (field, value) in said.as_object().into_iter().flatten() {
                hello[field] = value.clone();
            }
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(server).unwrap();
                let ours = move |_: &str| resumes.clone();
                let established = establish(conn.into(), stream(), false, hello, "usb", &ours).unwrap();
                Fake { conn: established.conn, stream: established.stream, decoder: established.decoder,
                       udp: established.udp, udp_peer: established.udp_peer, sealer: established.sealer, next: 1 }
            })
        }

        fn send(&mut self, kind: u8, channel: u16, body: &[u8]) {
            self.try_send(kind, channel, body).unwrap();
        }

        /// As send(): an error once the PC let go of the stream.
        fn try_send(&mut self, kind: u8, channel: u16, body: &[u8]) -> io::Result<()> {
            let mut bytes = Vec::new();
            frame::encode(kind, channel, body, &mut bytes);
            self.conn.writer().write_all(&bytes)?;
            while self.conn.wants_write() {
                self.conn.write_tls(&mut self.stream)?;
            }
            Ok(())
        }

        /// A reliable frame, numbered (its hello was 0).
        fn reliable(&mut self, kind: u8, channel: u16, body: &[u8]) {
            let mut numbered = self.next.to_be_bytes().to_vec();
            numbered.extend_from_slice(body);
            self.next += 1;
            self.send(kind, channel, &numbered);
        }

        fn say(&mut self, message: Value) {
            self.reliable(frame::SESSION, 0, message.to_string().as_bytes());
        }

        /// A datagram to the PC's UDP socket, sealed.
        fn datagram(&self, channel: u16, body: &[u8]) {
            let mut sealed = Vec::new();
            self.sealer.seal(channel, body, &mut sealed);
            self.udp.as_ref().unwrap().send_to(&sealed, self.udp_peer.unwrap()).unwrap();
        }

        /// The PC's next message of one of these types (the others passed over).
        fn heard(&mut self, types: &[&str]) -> Value {
            loop {
                let Some((kind, _, body)) = read_frame(&mut self.conn, &mut self.stream, &mut self.decoder).unwrap()
                else { continue };
                if kind != frame::SESSION {
                    continue;
                }
                let message: Value = serde_json::from_slice(&body[8..]).unwrap();
                if types.iter().any(|t| message["t"] == *t) {
                    return message;
                }
            }
        }

        /// A control channel the PC opens for feature, taken here.
        fn accept(&mut self, session: &Arc<Session>, feature: &str) -> Channel {
            let opening = {
                let (session, feature) = (session.clone(), feature.to_owned());
                std::thread::spawn(move || session.open(&feature, "c", Class::Control))
            };
            let open = self.heard(&["open"]);
            self.say(json!({"t": "accept", "id": open["id"]}));
            opening.join().unwrap().unwrap()
        }
    }

    /// A PC, and the tablet it is paired with (faked).
    struct Devices {
        pc: Node,
        tablet: Identity,
        pc_fingerprint: String,
    }

    fn devices() -> Devices {
        let (pc_id, pc_fingerprint) = identity();
        let (tablet, tablet_fingerprint) = identity();
        let pc = Node::new("pc", &pc_id, trusting(&tablet_fingerprint)).unwrap();
        Devices { pc, tablet, pc_fingerprint }
    }

    impl Devices {
        /// A session (or a path for the one resumes names) on a cable: no
        /// UDP.
        fn by_cable(&self, flow: bool, resumes: Option<String>) -> (Arc<Session>, Fake) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let faking = Fake::start(&self.tablet, &self.pc_fingerprint, flow, resumes, move || Box::new(theirs));
            let session = self.pc.connect_stream(Box::new(ours), "usb").unwrap();
            (session, faking.join().unwrap())
        }

        /// A session (or a path for the one resumes names) on a stream of
        /// kind, the tablet's hello saying said's fields too: no UDP.
        fn by_stream(&self, said: Value, resumes: Option<(String, bool)>, kind: &str) -> (Arc<Session>, Fake) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let faking = Fake::start_saying(&self.tablet, &self.pc_fingerprint, said, resumes, move || Box::new(theirs));
            let session = self.pc.connect_stream(Box::new(ours), kind).unwrap();
            (session, faking.join().unwrap())
        }

        /// A session over TCP on loopback: UDP too, to a socket of the
        /// tablet's that sends nothing.
        fn by_tcp(&self) -> (Arc<Session>, Fake) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let faking = Fake::start(&self.tablet, &self.pc_fingerprint, true, None,
                                     move || Box::new(listener.accept().unwrap().0));
            let session = self.pc.connect(address, Duration::from_secs(2)).unwrap();
            (session, faking.join().unwrap())
        }
    }

    /// What node tells of its sessions' paths as each comes, and when;
    /// Detached as None.
    fn told_paths(node: &Node) -> mpsc::Receiver<(Instant, Option<PathInfo>)> {
        let (tell, told) = mpsc::channel();
        node.on_event(move |event| {
            let said = match event {
                Event::Path(_, path) => Some(path),
                Event::Detached(_) => None,
                _ => return,
            };
            let _ = tell.send((Instant::now(), said));
        });
        told
    }

    /// The next of told, the path's id and state (None: Detached).
    fn next_told(told: &mpsc::Receiver<(Instant, Option<PathInfo>)>) -> (Instant, Option<(u32, PathState)>, Option<PathInfo>) {
        let (at, path) = told.recv_timeout(Duration::from_secs(8)).expect("nothing told");
        (at, path.as_ref().map(|path| (path.id, path.state)), path)
    }

    /// This process's threads named so, each one's directory in /proc.
    fn threads(name: &str) -> Vec<PathBuf> {
        std::fs::read_dir("/proc/self/task").unwrap().flatten().map(|task| task.path())
            .filter(|task| std::fs::read_to_string(task.join("comm")).is_ok_and(|comm| comm.trim_end() == name))
            .collect()
    }

    /// How many times the threads named so went to sleep, all told.
    fn sleeps(name: &str) -> u64 {
        threads(name).iter().map(|task| {
            let status = std::fs::read_to_string(task.join("status")).unwrap_or_default();
            status.lines().find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
                .and_then(|count| count.trim().parse::<u64>().ok()).unwrap_or(0)
        }).sum()
    }

    /// Until done, or panics past within.
    fn until(what: &str, within: Duration, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < within, "{what}: not in {within:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The CPU time the threads named so took so far.
    fn cpu(name: &str) -> Duration {
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let ticks: u64 = threads(name).iter().map(|task| {
            let stat = std::fs::read_to_string(task.join("stat")).unwrap_or_default();
            // After the name: the state first, utime and stime 12th and 13th.
            let fields: Vec<&str> = stat.rsplit_once(')').map_or(Vec::new(), |(_, rest)| rest.split_whitespace().collect());
            fields.get(11..13).map_or(0, |times| times.iter().filter_map(|t| t.parse::<u64>().ok()).sum::<u64>())
        }).sum();
        Duration::from_millis(ticks * 1000 / tick)
    }

    #[test]
    fn a_session_waiting_for_acknowledgements_waits_not_spins() {
        // A tablet that takes every byte and acknowledges none (its pings say
        // 0): past UNACKED_MOST no reliable frame goes, and meanwhile the
        // session's thread sleeps (it went round with no wait, a core at
        // 100 %); acknowledged, the rest goes at once.
        let _one = one_at_a_time();
        let devices = devices();
        let (session, mut fake) = devices.by_cable(false, None);
        let channel = fake.accept(&session, "sink");
        // What comes from now on taken unread.
        let mut drain = fake.stream.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                match drain.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
        });
        let feeding = std::thread::spawn(move || {
            let chunk = vec![7u8; 200 << 10];
            for _ in 0..400 {  // 80 MB, more than UNACKED_MOST
                if channel.send(&chunk).is_err() {
                    break;
                }
            }
            channel
        });
        // Until the window is full: what waits to go stays as it is.
        let started = Instant::now();
        let mut seen = VecDeque::new();
        while seen.len() < 3 || seen.iter().any(|queued| *queued != seen[0]) || seen[0] == 0 {
            assert!(started.elapsed() < Duration::from_secs(10), "the window never full: {:?}", seen);
            std::thread::sleep(Duration::from_millis(100));
            seen.push_back(session.queued());
            if seen.len() > 3 {
                seen.pop_front();
            }
        }
        fake.send(frame::PING, 0, &0u64.to_be_bytes());  // heard: the path stays
        let before = cpu("link-session");
        std::thread::sleep(Duration::from_secs(1));
        let spent = cpu("link-session").saturating_sub(before);
        assert!(spent < Duration::from_millis(300), "{spent:?} of CPU in a second, waiting for an acknowledgement");
        fake.send(frame::PING, 0, &u64::MAX.to_be_bytes());
        drop(feeding.join().unwrap());
        let started = Instant::now();
        while session.queued() > 0 {
            assert!(started.elapsed() < Duration::from_secs(2), "{} bytes still waiting once acknowledged", session.queued());
            std::thread::sleep(Duration::from_millis(10));
        }
        devices.pc.close();
    }

    #[test]
    fn a_channel_closed_over_there_lets_go_of_its_threads_with_the_session() {
        // Flow on: the tablet fills a channel whose feature on the PC
        // reads nothing (its inbox waits for room in the feature's
        // socket), then closes it. Out of the session's channels, the
        // inbox and the pump (waiting for the feature, holding the
        // session) were out of reach of the session's end: both stayed as
        // long as the feature kept its end, unread.
        let _one = one_at_a_time();
        let devices = devices();
        let (session, mut fake) = devices.by_cable(true, None);
        let channel = fake.accept(&session, "sink");
        let end = session.channels.lock().unwrap()[&channel.id].clone();
        let chunk = vec![1u8; 200 << 10];
        fake.conn.set_buffer_limit(None);  // a frame bigger than rustls's 64 KiB at once
        let mut sent = 0;
        // Until the feature's socket is full: the inbox waits with one more.
        while end.taken.load(Ordering::Relaxed) == sent {
            assert!(sent < 64 << 20, "the feature's socket never full");
            fake.reliable(frame::MESSAGE, channel.id, &chunk);
            sent += chunk.len() as u64;
            until("delivered", Duration::from_secs(2), || end.received.load(Ordering::Relaxed) == sent);
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(end);
        fake.say(json!({"t": "close", "id": channel.id}));
        until("closed here too", Duration::from_secs(2), || session.channels() == 0);
        assert_eq!((threads("link-inbox").len(), threads("link-channel").len()), (1, 1), "the inbox and the pump");
        session.close();
        until("the inbox and the pump over with the session", Duration::from_secs(1),
              || threads("link-inbox").is_empty() && threads("link-channel").is_empty());
        drop(channel);
        devices.pc.close();
    }

    #[test]
    fn a_tablet_that_takes_nothing_piles_up_no_answers() {
        // It opens channels of a feature not served, ever faster than it
        // reads (nothing): each refusal waited in Queue::Ahead, behind a
        // stream that took nothing, and the path, heard all along, was
        // never lost: megabytes of them, for as long as it went on.
        let _one = one_at_a_time();
        let devices = devices();
        let (session, mut fake) = devices.by_cable(false, None);
        let mut frames = Vec::new();
        for i in 0..100_000u64 {
            let mut body = fake.next.to_be_bytes().to_vec();
            fake.next += 1;
            let open = json!({"t": "open", "id": 2 * (i % 30_000) + 2, "feature": "none", "channel": "x", "class": "control"});
            body.extend_from_slice(open.to_string().as_bytes());
            frame::encode(frame::SESSION, 0, &body, &mut frames);
        }
        fake.conn.set_buffer_limit(None);
        fake.conn.writer().write_all(&frames).unwrap();
        while fake.conn.wants_write() && fake.conn.write_tls(&mut fake.stream).is_ok() {}
        until("the path lost", Duration::from_secs(3), || session.path().is_none());
        assert!(session.queued() < 1 << 20, "{} bytes queued for a tablet that takes nothing", session.queued());
        devices.pc.close();
    }

    #[test]
    fn a_path_that_takes_nothing_is_lost_though_heard() {
        // A tablet that pings and reads nothing: what the PC sends fills
        // the stream and waits there. Heard all along, the path was never
        // lost (PATH_SILENCE watched only what came); now PATH_SILENCE
        // with nothing taken loses it, told as any lost path.
        let _one = one_at_a_time();
        let devices = devices();
        let told = told_paths(&devices.pc);
        let (session, mut fake) = devices.by_cable(false, None);
        let channel = fake.accept(&session, "sink");
        let feeding = std::thread::spawn(move || {
            let chunk = vec![7u8; 200 << 10];
            for _ in 0..20 {
                if channel.send(&chunk).is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        let mut failed = None;
        while failed.is_none() && started.elapsed() < PATH_SILENCE + Duration::from_secs(3) {
            if fake.try_send(frame::PING, 0, &0u64.to_be_bytes()).is_err() {
                break;
            }
            while let Ok((_, Some(path))) = told.recv_timeout(Duration::from_millis(500)) {
                if path.state == PathState::Failed {
                    failed = Some(started.elapsed());
                }
            }
        }
        let failed = failed.expect("a path that takes nothing never lost");
        assert!(failed > PATH_SILENCE - Duration::from_secs(1), "lost after {failed:?}");
        devices.pc.close();
        feeding.join().unwrap();
    }

    /// What node tells of its sessions' ends, as each comes: "detached",
    /// or why one is over (Gone).
    fn told_ends(node: &Node) -> mpsc::Receiver<String> {
        let (tell, told) = mpsc::channel();
        node.on_event(move |event| {
            let said = match event {
                Event::Detached(_) => "detached".to_owned(),
                Event::Gone(_, reason) => reason,
                _ => return,
            };
            let _ = tell.send(said);
        });
        told
    }

    /// session's thread, once its first ping is out (nothing more to write
    /// for a second), held at the top of its round (woken out of its poll)
    /// until what this gives is dropped: what fake does meanwhile is all
    /// there as it goes on.
    fn hold<'a>(session: &'a Session, fake: &mut Fake) -> std::sync::MutexGuard<'a, Option<Established>> {
        while !matches!(read_frame(&mut fake.conn, &mut fake.stream, &mut fake.decoder).unwrap(),
                        Some((frame::PING, _, _))) {}
        let held = session.arriving.lock().unwrap();
        session.wake.wake();
        std::thread::sleep(Duration::from_millis(100));
        held
    }

    #[test]
    fn a_bye_and_the_streams_end_at_once_end_the_session() {
        // The tablet says "bye" and lets go of its stream at once (TLS's
        // close_notify between or not): the end came in the same read as
        // the bye, and lost the path before the bye was taken (detached,
        // the session waited RESUME_WAIT for another, the PC reached for
        // the tablet again). Now the bye is taken first: the session is
        // over. With no bye, the path is lost as before.
        let _one = one_at_a_time();
        for (bye, notify) in [(true, false), (true, true), (false, false), (false, true)] {
            let devices = devices();
            let told = told_ends(&devices.pc);
            let (session, mut fake) = devices.by_cable(false, None);
            let held = hold(&session, &mut fake);
            if bye {
                fake.say(json!({"t": "bye"}));
            }
            if notify {
                fake.conn.send_close_notify();
                while fake.conn.wants_write() {
                    fake.conn.write_tls(&mut fake.stream).unwrap();
                }
            }
            drop(fake);
            drop(held);
            let said = told.recv_timeout(Duration::from_secs(2)).expect("nothing told");
            let expected = if bye { "closed by the other side" } else { "detached" };
            assert_eq!(said, expected, "bye: {bye}, close_notify: {notify}");
            assert_eq!(session.is_closed(), bye);
            devices.pc.close();
        }
    }

    #[test]
    fn a_bye_before_a_write_that_fails_ends_the_session() {
        // The tablet says "bye" and lets go of its stream while the PC has
        // a ping to send: the write fails (EPIPE) before anything is read,
        // and the path was lost with the bye unread (detached, the session
        // waited RESUME_WAIT for another). Now what came is taken first:
        // the session is over. With no bye, the path is lost as before.
        let _one = one_at_a_time();
        for bye in [true, false] {
            let devices = devices();
            let told = told_ends(&devices.pc);
            let (session, mut fake) = devices.by_cable(false, None);
            let held = hold(&session, &mut fake);
            session.queue(frame::PING, 0, &0u64.to_be_bytes());
            if bye {
                fake.say(json!({"t": "bye"}));
            }
            drop(fake);
            drop(held);
            let said = told.recv_timeout(Duration::from_secs(2)).expect("nothing told");
            let expected = if bye { "closed by the other side" } else { "detached" };
            assert_eq!(said, expected, "bye: {bye}");
            assert_eq!(session.is_closed(), bye);
            devices.pc.close();
        }
    }

    #[test]
    fn a_path_that_comes_as_the_session_ends_is_closed() {
        // A path made as the session ended: adopt() past its look at the
        // session (not over yet), end() then over, its paths ended and
        // those coming let go of, before this one came. Closed at once,
        // its stream shut down. (It stayed among those coming, its stream
        // open, until the session was dropped: on a cable, plus-one-link
        // waited that long for the accessory to be let go of.)
        let _one = one_at_a_time();
        let (pc_id, pc_fingerprint) = identity();
        let (tablet, tablet_fingerprint) = identity();
        let pc = Node::new("pc", &pc_id, trusting(&tablet_fingerprint)).unwrap();
        let said = json!({"paths": 1});
        let (first, theirs) = UnixStream::pair().unwrap();
        let faking = Fake::start_saying(&tablet, &pc_fingerprint, said.clone(), None, move || Box::new(theirs));
        let session = pc.connect_stream(Box::new(first), "bt").unwrap();
        let _bt = faking.join().unwrap();
        assert!(session.multipath);
        let (ours, theirs) = UnixStream::pair().unwrap();
        let faking =
            Fake::start_saying(&tablet, &pc_fingerprint, said, Some((session.id.clone(), false)), move || Box::new(theirs));
        let name = rustls::pki_types::ServerName::try_from("plus-one.invalid").unwrap();
        let conn = rustls::ClientConnection::new(pc_id.client_config(trusting(&tablet_fingerprint)).unwrap(), name).unwrap();
        let hello = json!({"t": "hello", "name": "pc", "features": {}, "flow": 1, "paths": 1});
        let id = session.id.clone();
        let established = establish(conn.into(), Box::new(ours), true, hello, "usb", &move |_| Some((id.clone(), false))).unwrap();
        let mut usb = faking.join().unwrap();
        session.close();
        // Where adopt() goes on past its look at the session.
        assert!(!session.join(established), "taken by a session over");
        usb.stream.set_timeouts(Some(Duration::from_secs(2))).unwrap();
        let mut byte = [0u8; 1];
        let read = std::io::Read::read(&mut usb.stream, &mut byte);
        assert!(matches!(read, Ok(0)), "its stream not shut down: {read:?}");
        pc.close();
    }

    /// fake pinging its path each 300 ms on a thread, until the sender is
    /// dropped or its stream fails: then the fake back, and whether it
    /// failed.
    fn pinging(mut fake: Fake) -> (mpsc::Sender<()>, std::thread::JoinHandle<(Fake, bool)>) {
        let (stop, stopped) = mpsc::channel::<()>();
        let pinging = std::thread::spawn(move || {
            while stopped.recv_timeout(Duration::from_millis(300)) == Err(mpsc::RecvTimeoutError::Timeout) {
                if fake.try_send(frame::PING, 0, &0u64.to_be_bytes()).is_err() {
                    return (fake, true);
                }
            }
            (fake, false)
        });
        (stop, pinging)
    }

    #[test]
    fn a_peer_from_before_has_one_path_at_a_time_as_before() {
        // A tablet whose hello says nothing of "paths": a better path takes
        // over, the one before ends once the tablet is heard on the new
        // one, the session has one path at a time; that one cut, the
        // session is detached, with nothing to go on with. The same tablet
        // saying "paths": 1 keeps the one before as a reserve (pinged), and
        // the session goes on over it.
        let _one = one_at_a_time();
        for paths in [false, true] {
            let devices = devices();
            let told = told_ends(&devices.pc);
            let said = if paths { json!({"paths": 1}) } else { json!({}) };
            let (session, bt) = devices.by_stream(said.clone(), None, "bt");
            let bt = pinging(bt);
            let (again, usb) = devices.by_stream(said, Some((session.id.clone(), false)), "usb");
            assert!(Arc::ptr_eq(&again, &session), "the same session, another path");
            let (stop_usb, usb) = pinging(usb);
            until("on usb", Duration::from_secs(5), || session.path().as_deref() == Some("usb"));
            let kinds = || session.paths().iter().map(|path| (path.kind.clone(), path.state.name())).collect::<Vec<_>>();
            if paths {
                assert_eq!(kinds(), [("usb".to_owned(), "active"), ("bt".to_owned(), "standby")]);
            } else {
                assert_eq!(kinds(), [("usb".to_owned(), "active")]);
                let (_, failed) = bt.1.join().unwrap();
                assert!(failed, "the bt path not ended");
            }
            drop(stop_usb);
            let (usb, _) = usb.join().unwrap();
            usb.stream.shutdown();
            if paths {
                until("on bt", Duration::from_secs(1), || session.path().as_deref() == Some("bt"));
                assert_eq!(told.recv_timeout(Duration::from_secs(1)).ok(), None, "detached");
                assert_eq!(kinds(), [("bt".to_owned(), "active")]);
                drop(bt.0);
            } else {
                assert_eq!(told.recv_timeout(Duration::from_secs(2)).as_deref(), Ok("detached"));
                assert!(session.paths().is_empty());
            }
            devices.pc.close();
        }
    }

    #[test]
    fn a_paths_datagrams_sleep_until_one_comes() {
        // UDP to a tablet that sends nothing on it: the thread reading the
        // path's datagrams never wakes (it woke each 500 ms to see whether
        // the path was over), and ends with the path all the same.
        let _one = one_at_a_time();
        let devices = devices();
        let (session, fake) = devices.by_tcp();
        until("the datagrams' thread", Duration::from_secs(1), || threads("link-datagrams").len() == 1);
        std::thread::sleep(Duration::from_millis(300));
        let before = sleeps("link-datagrams");
        std::thread::sleep(Duration::from_secs(2));
        let woke = sleeps("link-datagrams") - before;
        assert!(woke <= 1, "the datagrams' thread woke {woke} times in 2 s, with none coming");
        drop(fake);
        until("the path lost", Duration::from_secs(2), || session.path().is_none());
        until("the datagrams' thread over", Duration::from_millis(500), || threads("link-datagrams").is_empty());
        devices.pc.close();
    }

    #[test]
    fn a_session_away_sleeps_until_a_path_comes() {
        // Its path lost, a session waits RESUME_WAIT for another: asleep
        // meanwhile (it woke each 500 ms), and on the next as soon as it
        // comes.
        let _one = one_at_a_time();
        let devices = devices();
        let (session, fake) = devices.by_cable(true, None);
        drop(fake);
        until("the path lost", Duration::from_secs(2), || session.path().is_none());
        std::thread::sleep(Duration::from_millis(100));
        let before = sleeps("link-session");
        std::thread::sleep(Duration::from_secs(2));
        let woke = sleeps("link-session") - before;
        assert!(woke <= 1, "the session's thread woke {woke} times in 2 s, waiting for a path");
        let (again, _fake) = devices.by_cable(true, Some(session.id.clone()));
        assert!(Arc::ptr_eq(&again, &session), "the same session, another path");
        until("on the new path", Duration::from_millis(100), || session.path().is_some());
        devices.pc.close();
    }

    #[test]
    fn an_open_with_an_id_no_channel_can_have_is_refused() {
        // 65538 is no u16: refused as it came, not taken for channel 2.
        let _one = one_at_a_time();
        let devices = devices();
        devices.pc.serve("echo", 1, |_| {});
        let (session, mut fake) = devices.by_cable(false, None);
        fake.say(json!({"t": "open", "id": 65538, "feature": "echo", "channel": "x", "class": "control"}));
        let answer = fake.heard(&["accept", "refuse"]);
        assert_eq!((&answer["t"], &answer["id"], &answer["reason"]), (&json!("refuse"), &json!(65538), &json!("bad id")));
        assert_eq!(session.channels(), 0);
        devices.pc.close();
    }

    #[test]
    fn a_message_with_an_id_no_channel_can_have_touches_none() {
        // A close for 65537 (no u16) is no close of channel 1.
        let _one = one_at_a_time();
        let devices = devices();
        let (session, mut fake) = devices.by_cable(false, None);
        let channel = fake.accept(&session, "sink");
        assert_eq!(channel.id, 1);
        fake.say(json!({"t": "close", "id": 65537}));
        fake.reliable(frame::MESSAGE, 1, b"still");
        let mut buf = [0u8; 64];
        assert_eq!(channel.recv(&mut buf, Some(Duration::from_secs(5))).unwrap(), Some(5), "the channel closed");
        assert_eq!(session.channels(), 1);
        devices.pc.close();
    }

    #[test]
    fn a_silent_path_is_suspect_at_once_and_lost_as_before() {
        // A tablet that sends nothing, not even its pings: its path suspect
        // PATH_SUSPECT after it was last heard (told then, not at the next
        // ping, up to a second on), asleep meanwhile (its own pings alone
        // wake it); active again once heard; lost after PATH_SILENCE as
        // before, told before the session is detached.
        let _one = one_at_a_time();
        let devices = devices();
        let told = told_paths(&devices.pc);
        let (session, mut fake) = devices.by_cable(false, None);
        let (taken, path, info) = next_told(&told);
        assert_eq!(path, Some((1, PathState::Active)));
        assert_eq!(info.map(|info| (info.kind, info.transport)), Some(("usb".to_owned(), "stream")));
        let (at, path, _) = next_told(&told);
        assert_eq!(path, Some((1, PathState::Suspect)));
        let after = at.duration_since(taken);
        assert!(after >= Duration::from_millis(1400) && after < Duration::from_millis(1800), "suspect after {after:?}");
        assert_eq!(session.paths().iter().map(|path| path.state).collect::<Vec<_>>(), [PathState::Suspect]);
        let (before, spent) = (sleeps("link-session"), cpu("link-session"));
        std::thread::sleep(Duration::from_millis(1500));
        let woke = sleeps("link-session") - before;
        let spent = cpu("link-session").saturating_sub(spent);
        assert!(woke <= 4 && spent < Duration::from_millis(100), "suspect, it woke {woke} times in 1.5 s, {spent:?} of CPU");
        fake.send(frame::PING, 0, &0u64.to_be_bytes());
        let (heard, path, _) = next_told(&told);
        assert_eq!(path, Some((1, PathState::Active)));
        assert_eq!(next_told(&told).1, Some((1, PathState::Suspect)));
        let (lost, path, info) = next_told(&told);
        assert_eq!(path, Some((1, PathState::Failed)));
        let after = lost.duration_since(heard);
        assert!(after >= PATH_SILENCE && after < PATH_SILENCE + Duration::from_millis(1500), "lost after {after:?}");
        assert!(info.unwrap().silent_ms >= 5000);
        assert_eq!(next_told(&told).1, None, "then detached");
        assert!(session.paths().is_empty());
        devices.pc.close();
    }

    #[test]
    fn a_suspect_path_heard_by_a_datagram_is_active_at_once() {
        // Only the session's thread moves a path's state: the datagrams'
        // wakes it, and the path is active again at once, not at its next
        // ping (each second, past UDP_PROOF on a path whose UDP is not
        // proven: a second from now).
        let _one = one_at_a_time();
        let devices = devices();
        let told = told_paths(&devices.pc);
        let (_session, fake) = devices.by_tcp();
        let (taken, path, _) = next_told(&told);
        assert_eq!(path, Some((1, PathState::Active)));
        assert_eq!(next_told(&told).1, Some((1, PathState::Suspect)));
        std::thread::sleep((taken + UDP_PROOF + Duration::from_millis(300)).saturating_duration_since(Instant::now()));
        let sent = Instant::now();
        let mut ping = vec![PING];
        ping.extend_from_slice(&0u64.to_be_bytes());
        fake.datagram(DATAGRAM_PATH, &ping);
        let (heard, path, _) = next_told(&told);
        assert_eq!(path, Some((1, PathState::Active)));
        let after = heard.duration_since(sent);
        assert!(after < Duration::from_millis(300), "active again {after:?} after a datagram");
        devices.pc.close();
    }

    #[test]
    fn a_path_heard_wakes_for_its_pings_alone() {
        // A tablet that pings each second and does nothing else: the PC's
        // session wakes for its own pings and the tablet's, nothing more
        // (when the path would be suspect is in the wait it has, no timer
        // of its own).
        let _one = one_at_a_time();
        let devices = devices();
        let (session, mut fake) = devices.by_cable(false, None);
        let (stop, stopped) = mpsc::channel::<()>();
        let pinging = std::thread::spawn(move || {
            while stopped.recv_timeout(PING_EVERY) == Err(mpsc::RecvTimeoutError::Timeout) {
                fake.send(frame::PING, 0, &0u64.to_be_bytes());
            }
            fake
        });
        std::thread::sleep(Duration::from_millis(500));
        let before = sleeps("link-session");
        std::thread::sleep(Duration::from_secs(3));
        let woke = sleeps("link-session") - before;
        assert!(woke <= 8, "the session's thread woke {woke} times in 3 s, with pings alone");
        assert_eq!(session.paths().iter().map(|path| path.state).collect::<Vec<_>>(), [PathState::Active]);
        stop.send(()).unwrap();
        drop(pinging.join().unwrap());
        devices.pc.close();
    }

    /// One window of drain's, at its end (now): its socket took bytes, full
    /// at its last write or not, run dry in it or not; what it tells.
    fn window(drain: &mut Drain, now: Instant, bytes: usize, full: bool, dry: bool) -> Option<u64> {
        drain.wrote(bytes);
        drain.after(full, dry);
        drain.roll(now).expect("a window's end").1
    }

    #[test]
    fn a_streams_budget_is_what_it_drains_full_its_prior_until_then() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let near = |told: Option<u64>, rate: u64| told.is_some_and(|told| told.abs_diff(rate) <= 2);
        let mut drain = Drain::new(start, prior("bt"));
        assert_eq!(drain.budget, Some(BT_PRIOR));
        // Its socket's room filled (not full as the window began): no rate.
        assert_eq!(window(&mut drain, at(200), 200 << 10, true, false), None);
        // Full from start to end at 1 Mbit/s: told once a second of it.
        for ms in [400, 600, 800, 1000] {
            assert_eq!(window(&mut drain, at(ms), 25_000, true, false), None);
        }
        assert_eq!((drain.budget, drain.measured), (Some(BT_PRIOR), false));
        assert!(near(window(&mut drain, at(1200), 25_000, true, false), 125_000));
        // Run dry in it: nothing said of the path, and a second again.
        assert_eq!(window(&mut drain, at(1400), 5_000, true, true), None);
        for ms in [1600, 1800, 2000, 2200] {
            assert_eq!(window(&mut drain, at(ms), 10_000, true, false), None);
        }
        // 0.4 Mbit/s: under the prior, down at once.
        assert!(near(window(&mut drain, at(2400), 10_000, true, false), 50_000));
        // The last second of it: within PUBLISH_CHANGE of the last told,
        // not told again.
        assert_eq!(window(&mut drain, at(2600), 15_000, true, false), None);
        assert_eq!(drain.budget, Some(55_000));
        // A round that emptied TLS's buffer, more to go at once: the window
        // ends at the next, the socket full.
        drain.wrote(12_000);
        drain.after(false, false);
        assert_eq!(drain.roll(at(2800)), None);
        drain.after(true, false);
        assert!(near(drain.roll(at(2810)).expect("a window's end").1, 56_435));
        // Not full at its end, and the next not full as it began: no rate.
        assert_eq!(window(&mut drain, at(3010), 2_000, false, true), None);
        assert_eq!(window(&mut drain, at(3210), 30_000, true, false), None);
        assert!(drain.run.is_empty());
        // A stream with no prior (TCP's, paced: pace.rs): never a budget.
        let mut tcp = Drain::new(start, prior("lan"));
        for ms in (200..=1400).step_by(200) {
            assert_eq!(window(&mut tcp, at(ms), 1 << 20, true, false), None);
        }
        assert_eq!(tcp.budget, None);
    }

    #[test]
    fn the_cable_is_never_held() {
        // Pulled, it was the user's hand: plugged back, the carrier as soon
        // as heard throughout HYSTERESIS. The air's kinds, held (FLAP_HOLD).
        let mut paths = Paths { lives: Vec::new(), resumed: false, last: None, held: HashMap::new() };
        let now = Instant::now();
        paths.hold("usb", now);
        assert!(!paths.held("usb"));
        for kind in ["lan", "p2p", "ap", "bt", "relay"] {
            paths.hold(kind, now);
            assert!(paths.held(kind), "{kind}");
        }
    }

    #[test]
    fn a_cables_budget_measured_down_goes_back_up() {
        // The tablet reading slowly a second (the cable's stream full
        // throughout at 1 MB/s): its budget that, told at once. It reads
        // again (never full since, the apps under the budget): 2 s on, 1.5
        // times that, told, and so on each 2 s, up to its prior (12 MB/s). (Measured once, a cable's budget stayed what one
        // stall drained, the desktop under it for good.) Bluetooth's stays
        // what it drained: the air's.
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        for (kind, prior) in [("usb", 12_000_000u64), ("bt", BT_PRIOR)] {
            let drain = Drain::new(start, Some(prior));
            let mut drain = if probes(kind) { drain.probing(prior) } else { drain };
            let slow = prior / 12;  // 1 MB/s on the cable
            let mut told = Vec::new();
            for ms in (200..=1200).step_by(200) {
                told.extend(window(&mut drain, at(ms), (slow / 5) as usize, true, false).map(|t| (ms, t)));
            }
            assert_eq!(told, [(1200, slow)], "{kind}: measured");
            told.clear();
            for ms in (1400..=20_000).step_by(200) {
                told.extend(window(&mut drain, at(ms), 1000, false, true).map(|t| (ms, t)));
            }
            if kind == "usb" {
                // The last step, to the prior, within PUBLISH_CHANGE of the
                // last told: not told.
                assert_eq!(told, [(3200, 1_500_000), (5200, 2_250_000), (7200, 3_375_000), (9200, 5_062_500), (11200, 7_593_750),
                                  (13200, 11_390_625)]);
                assert_eq!((drain.budget, drain.measured), (Some(prior), false), "its prior again");
            } else {
                assert!(told.is_empty(), "{kind}: up again: {told:?}");
                assert_eq!((drain.budget, drain.measured), (Some(slow), true));
            }
        }
    }

    #[test]
    fn a_cable_lets_realtime_wait_what_it_drains() {
        // A cable: its prior (80 Mbit/s) its budget until measured. A
        // keyframe's burst fills its little room (the window full) though
        // it takes 390 KB/s alone: realtime may wait REALTIME_WAIT of the
        // budget, 1.5 MB, not of what its socket took (58 KB, under the
        // keyframe: its tail dropped each time). Full throughout for a
        // second at 2 MB/s (the tablet slow to read): its budget that, and
        // realtime waits REALTIME_WAIT of it.
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut cable = Drain::new(start, prior("usb"));
        assert_eq!((cable.budget, cable.wait_most()), (Some(USB_PRIOR), 1_500_000));
        cable.wrote(78_000);
        cable.after(true, false);
        assert_eq!(cable.roll(at(200)), Some((1_500_000, None)), "a keyframe's burst");
        cable.wrote(5_000);
        cable.after(false, true);
        assert_eq!(cable.roll(at(400)), Some((1_500_000, None)), "run dry");
        assert_eq!(window(&mut cable, at(600), 400_000, true, false), None);
        for ms in [800, 1000, 1200, 1400] {
            assert_eq!(window(&mut cable, at(ms), 400_000, true, false), None);
        }
        assert_eq!(window(&mut cable, at(1600), 400_000, true, false), Some(2_000_000));
        assert_eq!(cable.wait_most(), STREAM_BUDGET.max(300_000));
        cable.wrote(400_000);
        cable.after(true, false);
        assert_eq!(cable.roll(at(1800)).map(|(wait, _)| wait), Some(300_000));
        // Bluetooth's the same: what its socket took, its budget if more.
        let mut bt = Drain::new(start, prior("bt"));
        bt.wrote(10_000);
        bt.after(true, false);
        assert_eq!(bt.roll(at(200)).map(|(wait, _)| wait), Some((BT_PRIOR as f64 * 0.15) as usize));
    }

    #[test]
    fn a_cable_read_slowly_lets_realtime_wait_only_what_it_drains() {
        // The tablet reading 1 MB/s of a cable taken for 12 MB/s (USB 2's
        // prior), the stream full from the first window on: full at its end
        // only, a window may be a burst's (REALTIME_WAIT of the prior, 1.8
        // MB); full throughout, what its socket took is what it drains:
        // REALTIME_WAIT of that, 150 KB, at once. (1.8 MB, 1.8 s of what it
        // drained, until the drain was measured a second later.)
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut cable = Drain::new(start, Some(12_000_000)).probing(12_000_000);
        let mut waits = Vec::new();
        for ms in (200..=1000).step_by(200) {
            cable.wrote(200_000);
            cable.after(true, false);
            waits.extend(cable.roll(at(ms)).map(|(wait, _)| wait));
        }
        assert_eq!(waits.len(), 5);
        assert_eq!(waits[0], 1_800_000, "full at its end only");
        assert!(waits[1..].iter().all(|wait| wait.abs_diff(150_000) <= 1), "full throughout: {waits:?}");
        assert_eq!(cable.budget, Some(12_000_000), "not measured yet");
    }

    #[test]
    fn a_cables_budget_goes_up_only_while_it_is_not_full() {
        // Measured at 1 MB/s (the tablet reading slowly), then not full,
        // then full throughout again from before PROBE_AFTER is past: not
        // up meanwhile, measured again (1 MB/s) once a second of it went.
        // (It went up 1.5 times while full, the apps told to fill it more:
        // the probe against the drain.)
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let prior = 12_000_000u64;
        let slow = prior / 12;
        let mut cable = Drain::new(start, Some(prior)).probing(prior);
        let mut told = Vec::new();
        for ms in (200..=1200).step_by(200) {
            told.extend(window(&mut cable, at(ms), (slow / 5) as usize, true, false).map(|t| (ms, t)));
        }
        assert_eq!(told, [(1200, slow)], "measured");
        told.clear();
        for ms in (1400..=2600).step_by(200) {
            told.extend(window(&mut cable, at(ms), 1000, false, true).map(|t| (ms, t)));
        }
        for ms in (2800..=4000).step_by(200) {
            told.extend(window(&mut cable, at(ms), (slow / 5) as usize, true, false).map(|t| (ms, t)));
        }
        assert!(told.is_empty(), "up while full: {told:?}");
        assert_eq!((cable.budget, cable.measured), (Some(slow), true));
    }

    /// A cable the other side reads drains(t) bytes a second of (t: since
    /// start), for until, in steps of 10 ms: the apps sending 0.9 of its
    /// budget as it goes, realtime dropped past what may wait (as queue()
    /// drops it). The runs of drops (from, to), the probe's intervals said
    /// (when, which), and the budget at each window's end.
    #[allow(clippy::type_complexity)]
    fn slow_cable(drain: &mut Drain, start: Instant, until: Duration, drains: impl Fn(Duration) -> f64)
                  -> (Vec<(Duration, Duration)>, Vec<(Duration, Duration)>, Vec<(Duration, u64)>) {
        let step = Duration::from_millis(10);
        let (mut queued, mut wait) = (0f64, drain.wait_most() as f64);
        let (mut drops, mut said, mut budgets) = (Vec::<(Duration, Duration)>::new(), Vec::new(), Vec::new());
        let mut t = Duration::ZERO;
        while t < until {
            t += step;
            let sent = drain.budget.unwrap_or(0) as f64 * 0.9 * step.as_secs_f64();
            let taken = sent.min((wait - queued).max(0.0));
            if taken < sent - 1e-6 {
                if drops.last().is_some_and(|(_, to)| t - *to <= step) {
                    drops.last_mut().unwrap().1 = t;
                } else {
                    drops.push((t, t));
                }
            }
            queued += taken;
            let took = queued.min(drains(t) * step.as_secs_f64());
            queued -= took;
            drain.wrote(took as usize);
            drain.after(queued > 1e-6, queued <= 1e-6);
            if t.as_millis() % RATE_WINDOW.as_millis() == 0 {
                if let Some((new, _)) = drain.roll(start + t) {
                    wait = new as f64;
                }
                said.extend(drain.probe_note(start + t).map(|every| (t, every)));
                budgets.push((t, drain.budget.unwrap_or(0)));
            }
        }
        (drops, said, budgets)
    }

    #[test]
    fn a_cable_read_slowly_goes_up_less_and_less_often() {
        // The tablet reads 1 MB/s of a cable taken for 12 MB/s for 2
        // minutes, then 20 MB/s. Measured down to 1 MB/s, its budget went
        // up 1.5 times 2 s after each measure, was measured back down,
        // realtime dropped for 0.8 s every 4.4 s meanwhile (27 times in the
        // 2 minutes). Now up again 4 s after, then 8, 16, 32, 32 (7 times),
        // said as that changes (10 s apart at least); the tablet fast
        // again, up the next time, held, then each 2 s to the prior, and
        // nothing dropped.
        let start = Instant::now();
        let prior = 12_000_000u64;
        let mut cable = Drain::new(start, Some(prior)).probing(prior);
        let slow = Duration::from_secs(120);
        let (drops, said, budgets) =
            slow_cable(&mut cable, start, Duration::from_secs(180), |t| if t <= slow { 1e6 } else { 20e6 });
        let starts: Vec<f64> = drops.iter().filter(|(from, _)| *from < slow).map(|(from, _)| from.as_secs_f64()).collect();
        let gaps: Vec<f64> = starts.windows(2).map(|two| two[1] - two[0]).collect();
        assert!(starts.len() <= 8, "dropped {} times in 2 minutes: {starts:?}", starts.len());
        assert!(gaps.windows(2).all(|two| two[1] + 0.3 >= two[0]) && gaps.last().is_some_and(|gap| *gap >= 32.0),
                "the drops not ever rarer: {gaps:?}");
        assert!(drops.iter().all(|(from, to)| *to - *from < Duration::from_millis(2500)), "{drops:?}");
        let intervals: Vec<u64> = said.iter().map(|(_, every)| every.as_secs()).collect();
        assert_eq!(intervals, [4, 8, 16, 32, 2], "said: {said:?}");
        assert!(said.windows(2).all(|two| two[1].0 - two[0].0 >= PROBE_SAID), "said: {said:?}");
        assert!(drops.iter().all(|(from, _)| *from < slow + Duration::from_secs(1)), "dropped with the tablet fast: {drops:?}");
        let again = budgets.iter().find(|(t, budget)| *t > slow && *budget == prior).map(|(t, _)| *t);
        assert!(again.is_some_and(|t| t < slow + PROBE_MOST + Duration::from_secs(12)), "the prior again at {again:?}");
    }
}
