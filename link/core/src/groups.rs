//! Realtime in groups on QUIC (lib.rs Class::Video, session/grouped.rs):
//! each group (a video's frame) on a unidirectional stream of its own, its
//! messages sealed as the datagrams are, each after its length (2 bytes,
//! big-endian). The datagrams (sound, input) still go first in each packet
//! (quinn writes them before any stream's data); the groups go ahead of the
//! session's stream, the older ahead of the newer (their priorities: what
//! QUIC sends again of a frame goes before the next frame), a key group
//! ahead of all. QUIC sends a group's lost data again, within the
//! controller's rate.
//!
//! A group is late LATE after its first message was handed over (the
//! frame's capture, near enough), but a channel's newest group never is (on
//! a still desktop it is the screen for a long time): once a newer one
//! begins, a late group is reset if not all acknowledged, at once when a
//! newer key group begins (superseded), and when the other side gives it up
//! (stopped). What of it is not sent then never is, nor is its lost data
//! sent again, and a late frame holds up no newer one. (After moq-dev/moq's
//! and moxygen's groups, checked against quinn-proto 0.11.19.)
//!
//! The other side reads each stream as it comes (a task each) and hands the
//! messages on as datagrams (read()) in the order of the streams' numbers,
//! the order their groups began (one writer opens them): a stream's messages
//! wait until every older one is over (finished, reset, the control stream);
//! the oldest's go as they come (Order). The app takes a frame whole after a
//! newer one for lost (DisplaySession.kt): with each task handing its own
//! messages on, a newer frame's reached it ahead of an older one's tail (sent
//! again, or a thread behind another), 75 frames lost in 10 minutes live
//! against 2 to 6 as datagrams. A message waits LATE at most: then the older
//! streams still open are given up (stopped: their sender resets them) and
//! what still comes on them is dropped; so too past HELD_MOST bytes waiting.
//! Each side says its groups' counts once a second while they go (Seconds).
//!
//! Beside them, a stream of the session's own messages, copies of those it
//! numbers (open, accept, credit...: session.rs early), ahead of all
//! (CONTROL); its first bytes are CONTROL_MARK, which no group's can be.
//! quinn's send window is one for all streams: the bulk, handed a little over
//! its share, keeps it full, and a group's message or a control frame waited
//! for room a round trip at least (the window frees what is acknowledged),
//! often past LATE. The window is opened for them as they are written
//! (write_now), never waited for.

use crate::cc::{CcConfig, Losses};
use crate::log;
use crate::quic::LATE;
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream, VarInt};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::Instant;

/// Group streams the other side may have open towards us at once: at 60
/// frames a second, LATE and a long path's round trip (200 ms) hold about
/// 18; some more than three times that. One more: the control stream.
pub(crate) const MAX_STREAMS: u32 = 64;
pub(crate) const UNI_STREAMS: u32 = MAX_STREAMS + 1;
/// The control stream's first bytes: as a length, more than any sealed
/// message's (MAX_SEALED).
const CONTROL_MARK: [u8; LENGTH] = [0xff, 0xff];
/// The streams' priorities in quinn (the higher first; the datagrams go
/// ahead of all): the control stream; the session's while it carries no
/// bulk (quic.rs bridge); a key group; the other groups, the older the
/// higher (what is sent again of a frame before the next frame); the
/// session's with the bulk.
pub(crate) const CONTROL: i32 = i32::MAX;
pub(crate) const SESSION_AHEAD: i32 = i32::MAX - 1;
const KEY_GROUP: i32 = i32::MAX - 2;
pub(crate) const SESSION_BEHIND: i32 = 0;
/// quinn's send window while a group's message or a control frame is
/// written into it (write_now): no limit but the other side's.
const OPEN: u64 = u64::MAX >> 2;
/// Messages handed over and not on their streams yet, at most (a fifth of
/// a second of a 100 Mbit/s video; more are dropped).
const QUEUE: usize = 2048;
/// A message's length before it on its stream.
const LENGTH: usize = 2;
const MAX_SEALED: usize = crate::session::MAX_DATAGRAM + crate::seal::OVERHEAD;
/// The groups each channel began last, kept to know whether a group is
/// its newest (a group not among them is long late): more than may be open
/// at once (MAX_STREAMS), and those waiting for their turn.
const HISTORY: usize = 2 * MAX_STREAMS as usize;
/// Bytes of newer groups waiting for an older one on the other side, at
/// most (Order): LATE of 300 Mbit/s, near enough.
const HELD_MOST: usize = 4 << 20;
/// Counts said once a second at most (Seconds).
const SECOND: Duration = Duration::from_secs(1);

/// A QUIC connection's groups so far (each connection has its own).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Groups begun on streams.
    pub groups: u64,
    /// Groups reset late, or superseded by a newer key group (or never
    /// begun for it), and the bytes written on them.
    pub late: u64,
    pub late_bytes: u64,
    pub superseded: u64,
    pub superseded_bytes: u64,
    /// Of those, the ones reset before all their messages were written.
    pub late_unsent: u64,
    /// Groups the other side gave up (stopped: a newer one waited LATE for
    /// it there, Order), reset then.
    pub stopped: u64,
    /// Streams reset, for any of those (not those acknowledged already).
    pub resets: u64,
    /// Messages dropped before their stream: the queue full, their group
    /// over before it began, or while they waited.
    pub dropped: u64,
    /// Groups whose stream waited for the other side's credit
    /// (MAX_STREAMS open already).
    pub credit_waits: u64,
    /// Messages and control frames that found quinn's send window full (the
    /// bulk's): it was opened for them. And how long they waited to be
    /// taken, all told (the other side's flow control).
    pub window_full: u64,
    pub wait_ms: u64,
    /// The session's messages that went ahead on the control stream.
    pub control: u64,
    /// Bytes written on group streams, and sent as datagrams (realtime and
    /// input not in groups, the path's pings).
    pub stream_bytes: u64,
    pub datagram_bytes: u64,
    /// Bytes in the packets QUIC declared lost: all (the groups', which it
    /// sends again unless reset; the datagrams'; the session stream's),
    /// those within 500 ms after a cut of the controller's, and its cuts
    /// (cc.rs Losses).
    pub lost_bytes: u64,
    pub lost_after_cut: u64,
    pub cuts: u64,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kb = |bytes: u64| bytes as f64 / 1000.0;
        write!(f, "groups={} late={} late_kb={:.1} superseded={} superseded_kb={:.1} late_unsent={} stopped={} \
                   resets={} dropped={} credit_waits={} window_full={} wait_ms={} control={} stream_kb={:.1} \
                   datagram_kb={:.1} lost_kb={:.1} lost_cut_kb={:.1} cuts={}",
               self.groups, self.late, kb(self.late_bytes), self.superseded, kb(self.superseded_bytes),
               self.late_unsent, self.stopped, self.resets, self.dropped, self.credit_waits, self.window_full,
               self.wait_ms, self.control, kb(self.stream_bytes), kb(self.datagram_bytes), kb(self.lost_bytes),
               kb(self.lost_after_cut), self.cuts)
    }
}

impl Stats {
    /// What moved from before over a span, as the log's line, if groups
    /// went in it (began, or were reset or given up).
    pub(crate) fn line(&self, before: &Stats, over: Duration) -> Option<String> {
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let begun = d(self.groups, before.groups);
        let (late, superseded, stopped) =
            (d(self.late, before.late), d(self.superseded, before.superseded), d(self.stopped, before.stopped));
        if begun + late + superseded + stopped == 0 {
            return None;
        }
        Some(format!("groups over {:.1} s: {begun} begun, {late} late ({} unsent), {superseded} superseded, \
                      {stopped} given up there, {} reset; {} messages dropped, {} credit waits, {} window full; \
                      {:.0} KB on streams, {:.1} KB lost",
                     over.as_secs_f64(), d(self.late_unsent, before.late_unsent), d(self.resets, before.resets),
                     d(self.dropped, before.dropped), d(self.credit_waits, before.credit_waits),
                     d(self.window_full, before.window_full),
                     d(self.stream_bytes, before.stream_bytes) as f64 / 1000.0,
                     d(self.lost_bytes, before.lost_bytes) as f64 / 1000.0))
    }
}

/// Counts said once a second at most while they move, as the other
/// per-second lines are (quic.rs Drops): what moved since the last said.
/// Nothing to wake for while nothing moves.
pub(crate) struct Seconds<T> {
    since: std::time::Instant,
    said: T,
}

impl<T: Copy + PartialEq> Seconds<T> {
    pub(crate) fn new(now: std::time::Instant, counts: T) -> Seconds<T> {
        Seconds { since: now, said: counts }
    }

    /// When the next line is due: a second after the last, if the counts
    /// moved since.
    pub(crate) fn due(&self, counts: &T) -> Option<std::time::Instant> {
        (*counts != self.said).then(|| self.since + SECOND)
    }

    /// The last second's line (line: the counts now, as last said, over
    /// how long), once it is over and if they moved in it.
    pub(crate) fn note(&mut self, now: std::time::Instant, counts: T,
                       line: impl FnOnce(&T, &T, Duration) -> Option<String>) -> Option<String> {
        if counts == self.said {
            self.since = now;
            return None;
        }
        let over = now.saturating_duration_since(self.since);
        if over < SECOND {
            return None;
        }
        let said = line(&counts, &self.said, over);
        (self.since, self.said) = (now, counts);
        said
    }
}

/// A message handed over: its channel, its group, whether it is the last
/// and its group a key one, it sealed and after its length.
pub(crate) struct Piece {
    channel: u16,
    group: u32,
    last: bool,
    key: bool,
    framed: Bytes,
}

/// A connection's groups: the writer (started with the first), the
/// control stream's (the same), what they share with the session's stream
/// (quic.rs bridge).
pub(crate) struct Groups {
    writer: OnceLock<mpsc::Sender<Piece>>,
    control: OnceLock<mpsc::UnboundedSender<Bytes>>,
    pub(crate) shared: Arc<Shared>,
}

impl Groups {
    /// A connection's, its controller cc's.
    pub(crate) fn new(cc: &CcConfig) -> Groups {
        Groups { writer: OnceLock::new(), control: OnceLock::new(), shared: Arc::new(Shared::new(cc.losses.clone())) }
    }
}

/// A group as it began: when its first message was handed over.
struct Begun {
    group: u32,
    at: std::time::Instant,
    key: bool,
}

/// Why a group is over.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Over {
    Late,
    Superseded,
    /// The other side gave it up (stopped it).
    Stopped,
}

/// The counts the writers and their streams' tasks keep.
#[derive(Default)]
struct Counts {
    groups: AtomicU64,
    late: AtomicU64,
    late_bytes: AtomicU64,
    superseded: AtomicU64,
    superseded_bytes: AtomicU64,
    late_unsent: AtomicU64,
    stopped: AtomicU64,
    resets: AtomicU64,
    dropped: AtomicU64,
    credit_waits: AtomicU64,
    window_full: AtomicU64,
    wait_us: AtomicU64,
    control: AtomicU64,
    stream_bytes: AtomicU64,
}

pub(crate) struct Shared {
    /// quinn's send window as last set (quic.rs bridge; write_now opens it
    /// and sets it back), under its lock.
    pub(crate) window: Mutex<u64>,
    /// Each channel's last HISTORY groups as they began (handed over), the
    /// newest last; told as a group begins.
    begun: Mutex<HashMap<u16, VecDeque<Begun>>>,
    began: Notify,
    /// The controller's losses.
    losses: Arc<Losses>,
    /// Bytes written on group streams not all acknowledged yet nor reset
    /// (a stream's count until it is): quinn's send window holds them on
    /// top of the session stream's share (quic.rs send_window), which stays
    /// the bulk's whatever the video holds.
    unacked: AtomicU64,
    /// Bytes handed over, and not written yet.
    handed: AtomicU64,
    queued: AtomicU64,
    count: Counts,
}

impl Shared {
    fn new(losses: Arc<Losses>) -> Shared {
        Shared { window: Mutex::new(crate::quic::SEND_WINDOW), begun: Mutex::default(), began: Notify::new(),
                 losses, unacked: AtomicU64::new(0), handed: AtomicU64::new(0), queued: AtomicU64::new(0),
                 count: Counts::default() }
    }

    /// quinn's send window set to window (the bridge, as the rate moves).
    pub(crate) fn set_window(&self, conn: &Connection, window: u64) {
        let mut set = self.window.lock().unwrap();
        *set = window;
        conn.set_send_window(window);
    }

    /// As much of data as quinn takes on stream now: if its send window is
    /// full (the bulk's, mostly: one for all streams), opened for this write
    /// alone and set back at once (what the bulk may take stays its share);
    /// None if quinn takes none of it still (the other side's flow
    /// control), and then cx is woken when it may.
    fn poll_write_now(&self, conn: &Connection, stream: &mut SendStream, data: &[u8], cx: &mut std::task::Context)
                      -> Option<Result<usize, quinn::WriteError>> {
        if let Poll::Ready(written) = Pin::new(&mut *stream).poll_write(cx, data) {
            return Some(written);
        }
        self.count.window_full.fetch_add(1, Ordering::Relaxed);
        let window = self.window.lock().unwrap();
        conn.set_send_window(OPEN);
        let written = Pin::new(&mut *stream).poll_write(cx, data);
        conn.set_send_window(*window);
        match written {
            Poll::Ready(written) => Some(written),
            Poll::Pending => None,
        }
    }

    /// Bytes on group streams not all acknowledged yet (unacked).
    pub(crate) fn unacked(&self) -> u64 {
        self.unacked.load(Ordering::Relaxed)
    }

    /// Bytes handed over to groups so far.
    pub(crate) fn handed(&self) -> u64 {
        self.handed.load(Ordering::Relaxed)
    }

    /// Ours and the controller's losses (the datagrams' bytes are the
    /// connection's: quic.rs).
    pub(crate) fn stats(&self) -> Stats {
        let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
        let c = &self.count;
        Stats { groups: load(&c.groups), late: load(&c.late), late_bytes: load(&c.late_bytes),
                superseded: load(&c.superseded), superseded_bytes: load(&c.superseded_bytes),
                late_unsent: load(&c.late_unsent), stopped: load(&c.stopped), resets: load(&c.resets),
                dropped: load(&c.dropped), credit_waits: load(&c.credit_waits), window_full: load(&c.window_full),
                wait_ms: load(&c.wait_us) / 1000, control: load(&c.control), stream_bytes: load(&c.stream_bytes),
                datagram_bytes: 0, lost_bytes: load(&self.losses.lost), lost_after_cut: load(&self.losses.after_cut),
                cuts: load(&self.losses.cuts) }
    }

    /// A message of channel's group handed over: if it is the group's first,
    /// the group began.
    fn begin(&self, channel: u16, group: u32, key: bool) {
        {
            let mut begun = self.begun.lock().unwrap();
            let groups = begun.entry(channel).or_default();
            if groups.back().is_some_and(|b| b.group == group) {
                return;
            }
            groups.push_back(Begun { group, at: std::time::Instant::now(), key });
            if groups.len() > HISTORY {
                groups.pop_front();
            }
        }
        self.began.notify_waiters();
    }

    /// Until when channel's group lasts: Ok(None) while it is the newest,
    /// Ok(LATE after it began) once a newer one did, or why it is over
    /// already (a newer key group began; long gone from the history).
    fn lasts(&self, channel: u16, group: u32) -> Result<Option<Instant>, Over> {
        let begun = self.begun.lock().unwrap();
        let Some(groups) = begun.get(&channel) else { return Ok(None) };
        let Some(i) = groups.iter().rposition(|b| b.group == group) else { return Err(Over::Late) };
        if groups.iter().skip(i + 1).any(|b| b.key) {
            return Err(Over::Superseded);
        }
        Ok((i + 1 < groups.len()).then(|| Instant::from_std(groups[i].at) + LATE))
    }

    /// A group over, its stream (if any) reset: counted (unsent: before all
    /// its messages were written).
    fn over(&self, why: Over, stream: Option<&mut SendStream>, bytes: u64, unsent: bool) {
        if let Some(stream) = stream {
            // Reset: what of it is not sent never is, nor is what was lost
            // sent again (an error: all of it acknowledged already). quinn
            // goes on sending a stream the other side stopped until it is.
            if stream.reset(VarInt::from_u32(0)).is_err() {
                return;
            }
            self.count.resets.fetch_add(1, Ordering::Relaxed);
        }
        let c = &self.count;
        let (count, of) = match why {
            Over::Late => (&c.late, Some(&c.late_bytes)),
            Over::Superseded => (&c.superseded, Some(&c.superseded_bytes)),
            Over::Stopped => (&c.stopped, None),
        };
        count.fetch_add(1, Ordering::Relaxed);
        if let Some(of) = of {
            of.fetch_add(bytes, Ordering::Relaxed);
        }
        if unsent && why != Over::Stopped {
            c.late_unsent.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// what, until channel's group is over (Shared::lasts): its output, or why
/// the group is over.
async fn until_over<T>(shared: &Shared, channel: u16, group: u32, what: impl Future<Output = T>) -> Result<T, Over> {
    tokio::pin!(what);
    loop {
        let began = shared.began.notified();
        tokio::pin!(began);
        began.as_mut().enable();
        let lasts = shared.lasts(channel, group)?;
        if lasts.is_some_and(|until| until <= Instant::now()) {
            return Err(Over::Late);
        }
        tokio::select! {
            biased;
            done = &mut what => return Ok(done),
            _ = &mut began => {}  // another group began: this one's end may be known now
            _ = sleep_until(lasts) => return Err(Over::Late),
        }
    }
}

async fn sleep_until(until: Option<Instant>) {
    match until {
        Some(until) => tokio::time::sleep_until(until).await,
        None => std::future::pending().await,
    }
}

impl Groups {
    /// A sealed message of channel's group (key: a key group; last: the
    /// group's last), to its stream: false if dropped (too many waiting).
    /// start: the writer started, with the first.
    pub(crate) fn send(&self, channel: u16, group: u32, key: bool, last: bool, sealed: &[u8],
                       start: impl FnOnce(mpsc::Receiver<Piece>)) -> bool {
        let writer = self.writer.get_or_init(|| {
            let (writer, pieces) = mpsc::channel(QUEUE);
            start(pieces);
            writer
        });
        self.shared.begin(channel, group, key);
        let mut framed = Vec::with_capacity(LENGTH + sealed.len());
        framed.extend_from_slice(&(sealed.len() as u16).to_be_bytes());
        framed.extend_from_slice(sealed);
        let length = framed.len() as u64;
        self.shared.queued.fetch_add(length, Ordering::Relaxed);
        self.shared.handed.fetch_add(length, Ordering::Relaxed);
        if writer.try_send(Piece { channel, group, last, key, framed: framed.into() }).is_err() {
            self.shared.queued.fetch_sub(length, Ordering::Relaxed);
            self.shared.count.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Bytes handed over and not written yet.
    pub(crate) fn queued(&self) -> usize {
        self.shared.queued.load(Ordering::Relaxed) as usize
    }
}

/// A channel's group being written.
struct Writing {
    group: u32,
    /// Its stream; None once finished or over, or if it never began (its
    /// messages that still come are dropped).
    stream: Option<SendStream>,
    /// Bytes written on it (counted in Shared::unacked until it is all
    /// acknowledged or reset).
    bytes: u64,
}

impl Writing {
    /// Its last message written, or the next group began: its stream
    /// finished, kept until all of it is acknowledged, or reset once over
    /// or given up by the other side.
    fn finish(&mut self, channel: u16, shared: &Arc<Shared>) {
        let Some(mut stream) = self.stream.take() else { return };
        let _ = stream.finish();
        let (group, bytes, shared) = (self.group, self.bytes, shared.clone());
        tokio::spawn(async move {
            let acknowledged = stream.stopped();
            let acknowledged = until_over(&shared, channel, group, acknowledged).await;
            match acknowledged {
                Ok(Ok(Some(_))) => shared.over(Over::Stopped, Some(&mut stream), bytes, false),
                Ok(_) => {}  // all acknowledged (or the connection over)
                Err(why) => shared.over(why, Some(&mut stream), bytes, false),
            }
            shared.unacked.fetch_sub(bytes, Ordering::Relaxed);
        });
    }

    /// Over before its last message: its stream reset (counted), its bytes
    /// no longer held.
    fn over(&mut self, why: Over, shared: &Shared) {
        if let Some(mut stream) = self.stream.take() {
            shared.over(why, Some(&mut stream), self.bytes, true);
            shared.unacked.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}

/// The writer of a connection's groups (on its runtime), until the
/// connection's Groups is gone: each message to its group's stream, a
/// stream opened for each new group (the one before finished). The counts
/// said once a second while groups go.
pub(crate) async fn write(conn: Connection, mut pieces: mpsc::Receiver<Piece>, shared: Arc<Shared>) {
    let mut writing: HashMap<u16, Writing> = HashMap::new();
    let mut opened = 0u32;
    let mut seconds = Seconds::new(std::time::Instant::now(), shared.stats());
    loop {
        let due = seconds.due(&shared.stats()).map(Instant::from_std);
        let piece = tokio::select! {
            piece = pieces.recv() => match piece {
                Some(piece) => Some(piece),
                None => break,
            },
            _ = sleep_until(due) => None,
        };
        if let Some(line) = seconds.note(std::time::Instant::now(), shared.stats(), Stats::line) {
            log!("{}: {line}", conn.remote_address());
        }
        let Some(piece) = piece else { continue };
        let (channel, group) = (piece.channel, piece.group);
        let length = piece.framed.len() as u64;
        shared.queued.fetch_sub(length, Ordering::Relaxed);
        if writing.get(&channel).is_none_or(|w| w.group != group) {
            // A new group: the one before finished.
            if let Some(mut before) = writing.remove(&channel) {
                before.finish(channel, &shared);
            }
            // (Waiting for the other side's credit, counted: MAX_STREAMS
            // too few.)
            let opening = conn.open_uni();
            tokio::pin!(opening);
            let at_once = tokio::select! {
                biased;
                stream = &mut opening => Some(stream),
                _ = std::future::ready(()) => None,
            };
            let open = match at_once {
                Some(stream) => Ok(stream),
                None => {
                    shared.count.credit_waits.fetch_add(1, Ordering::Relaxed);
                    until_over(&shared, channel, group, opening).await
                }
            };
            let stream = match open {
                Ok(Ok(stream)) => {
                    // A key group ahead of the others, those the older the
                    // higher (frames in order, what is sent again of one
                    // before the next), all ahead of the session's stream
                    // with the bulk (CONTROL...).
                    opened = opened.wrapping_add(1);
                    let priority = if piece.key { KEY_GROUP } else { KEY_GROUP - 1 - (opened & 0x3fff_ffff) as i32 };
                    let _ = stream.set_priority(priority);
                    shared.count.groups.fetch_add(1, Ordering::Relaxed);
                    Some(stream)
                }
                Ok(Err(_)) => None,  // the connection over
                Err(why) => {
                    shared.over(why, None, 0, true);
                    None
                }
            };
            writing.insert(channel, Writing { group, stream, bytes: 0 });
        }
        let Some(w) = writing.get_mut(&channel) else { continue };
        let over = shared.lasts(channel, group).and_then(|until| match until {
            Some(until) if until <= Instant::now() => Err(Over::Late),
            _ => Ok(()),
        });
        if let Err(why) = over {
            w.over(why, &shared);
        }
        let Some(stream) = w.stream.as_mut() else {
            shared.count.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // Held in quinn's send window on top of the session stream's share
        // (quic.rs send_window) until acknowledged: room the bulk leaves.
        w.bytes += length;
        shared.unacked.fetch_add(length, Ordering::Relaxed);
        if let Err(why) = write_all(&conn, stream, &piece.framed, channel, group, &shared).await {
            w.over(why, &shared);
            shared.count.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        shared.count.stream_bytes.fetch_add(length, Ordering::Relaxed);
        if piece.last {
            w.finish(channel, &shared);
        }
    }
    for (channel, mut w) in writing {
        w.finish(channel, &shared);
    }
}

/// As much of data as quinn takes on stream now (Shared::poll_write_now).
async fn write_now(conn: &Connection, stream: &mut SendStream, data: &[u8], shared: &Shared)
                   -> Option<Result<usize, quinn::WriteError>> {
    std::future::poll_fn(|cx| Poll::Ready(shared.poll_write_now(conn, stream, data, cx))).await
}

/// data on stream, all of it, unless its group is over first (why), the
/// other side stopped it (Stopped), or the stream or the connection is
/// over (Late); quinn's send window opened for it (write_now); waited for
/// only while the other side's flow control holds it.
async fn write_all(conn: &Connection, stream: &mut SendStream, mut data: &[u8], channel: u16, group: u32,
                   shared: &Shared) -> Result<(), Over> {
    while !data.is_empty() {
        let written = match write_now(conn, stream, data, shared).await {
            Some(written) => written,
            None => {
                let since = Instant::now();
                let written = until_over(shared, channel, group, stream.write(data)).await;
                shared.count.wait_us.fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
                written?
            }
        };
        let written = written.map_err(|e| match e {
            quinn::WriteError::Stopped(_) => Over::Stopped,
            _ => Over::Late,
        })?;
        data = &data[written..];
    }
    Ok(())
}

impl Groups {
    /// A copy of frames the session numbered (its own messages: session.rs
    /// early) on the control stream, ahead of all; started with the first.
    pub(crate) fn send_control(&self, frames: &[u8], start: impl FnOnce(mpsc::UnboundedReceiver<Bytes>)) {
        let writer = self.control.get_or_init(|| {
            let (writer, frames) = mpsc::unbounded_channel();
            start(frames);
            writer
        });
        // Gone with the connection: the session's stream carries them.
        if writer.send(Bytes::copy_from_slice(frames)).is_ok() {
            self.shared.count.control.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The control stream's writer (on the connection's runtime), until the
/// connection's Groups is gone or the connection is over: the frames, in
/// order, after CONTROL_MARK; quinn's send window opened for them
/// (write_now), never reset (the session's stream carries each too).
pub(crate) async fn write_control(conn: Connection, mut frames: mpsc::UnboundedReceiver<Bytes>, shared: Arc<Shared>) {
    let Ok(mut stream) = conn.open_uni().await else { return };
    let _ = stream.set_priority(CONTROL);
    let mut mark = Some(CONTROL_MARK);
    while let Some(frame) = frames.recv().await {
        let framed = match mark.take() {
            Some(mark) => Bytes::from([&mark[..], &frame[..]].concat()),
            None => frame,
        };
        let mut data = &framed[..];
        while !data.is_empty() {
            let written = match write_now(&conn, &mut stream, data, &shared).await {
                Some(written) => written,
                None => {
                    let since = Instant::now();
                    let written = stream.write(data).await;
                    shared.count.wait_us.fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
                    written
                }
            };
            match written {
                Ok(n) => data = &data[n..],
                Err(_) => return,
            }
        }
    }
}

/// The other side's groups on a QUIC connection as they came (Order).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Received {
    /// Group streams read to their end, and those the other side reset
    /// before it (late or superseded there).
    pub groups: u64,
    pub reset: u64,
    /// Groups not all come yet when a newer one was whole: each lost to the
    /// app if handed on in that order (DisplaySession.kt).
    pub overtaken: u64,
    /// Groups whose messages waited for an older one to be over.
    pub held: u64,
    /// Older groups given up so (a newer one's message waited LATE, or too
    /// many bytes waited), and the messages that still came on them.
    pub given_up: u64,
    pub dropped: u64,
}

impl Received {
    /// What moved from before over a span, as the log's line, if groups
    /// came in it (held_most: the longest a stream's messages waited in it,
    /// held_bytes_most the most bytes waiting).
    fn line(&self, before: &Received, over: Duration, held_most: Duration, held_bytes_most: usize) -> Option<String> {
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let (groups, reset, given_up) =
            (d(self.groups, before.groups), d(self.reset, before.reset), d(self.given_up, before.given_up));
        if groups + reset + given_up == 0 {
            return None;
        }
        Some(format!("groups in over {:.1} s: {groups} whole, {reset} reset; {} not whole when a newer one was, \
                      {} held for an older one ({:.0} ms, {:.0} KB at most), {given_up} given up, \
                      {} messages dropped after",
                     over.as_secs_f64(), d(self.overtaken, before.overtaken), d(self.held, before.held),
                     held_most.as_secs_f64() * 1000.0, held_bytes_most as f64 / 1000.0,
                     d(self.dropped, before.dropped)))
    }
}

/// What a group stream's reader tells the orderer (read()).
enum Read {
    /// A stream taken (quinn hands them over in the order of their
    /// numbers), and its stop: dropped, the reader stops it.
    Taken(u64, oneshot::Sender<()>),
    Message(u64, Bytes),
    End(u64, End),
}

/// How a stream ended, for the orderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// Its last message read.
    Finished,
    /// Reset by the other side (late or superseded there), or broken.
    Cut,
    /// No group's: the control stream.
    Control,
}

/// A stream as the orderer knows it, while it or an older one is open.
#[derive(Default)]
struct Held {
    /// Its messages waiting for the older ones, since when the first.
    messages: Vec<Bytes>,
    since: Option<std::time::Instant>,
    end: Option<End>,
    stop: Option<oneshot::Sender<()>>,
    /// Counted in Received::overtaken already.
    overtaken: bool,
}

/// The other side's group streams put back in order, by their numbers: the
/// order their groups began. The oldest stream not over (the head) hands
/// its messages on as they come; a newer one's wait until every older one
/// is over (finished, reset, the control stream) or given up: once a
/// message waited LATE, or more than HELD_MOST bytes wait, the head is
/// given up (stopped: its stop dropped), and what still comes on it is
/// dropped.
#[derive(Default)]
struct Order {
    head: u64,
    streams: BTreeMap<u64, Held>,
    /// Bytes waiting.
    held: usize,
    stats: Received,
    /// The longest a stream's messages waited, and the most bytes waiting,
    /// since the last line.
    held_most: Duration,
    held_bytes_most: usize,
}

impl Order {
    fn taken(&mut self, index: u64, stop: oneshot::Sender<()>) {
        // (An older one than the head: given up already, stop dropped.)
        if index >= self.head {
            self.streams.entry(index).or_default().stop = Some(stop);
        }
    }

    fn message<F: FnMut(Bytes)>(&mut self, index: u64, message: Bytes, now: std::time::Instant, on: &mut F) {
        if index < self.head {
            self.stats.dropped += 1;
            return;
        }
        if index == self.head {
            return on(message);
        }
        let stream = self.streams.entry(index).or_default();
        stream.since.get_or_insert(now);
        self.held += message.len();
        self.held_bytes_most = self.held_bytes_most.max(self.held);
        stream.messages.push(message);
    }

    fn end<F: FnMut(Bytes)>(&mut self, index: u64, end: End, now: std::time::Instant, on: &mut F) {
        if index < self.head {
            return;
        }
        self.streams.entry(index).or_default().end = Some(end);
        match end {
            End::Finished => self.stats.groups += 1,
            End::Cut => self.stats.reset += 1,
            End::Control => {}
        }
        if end == End::Finished && index > self.head {
            // The older ones still open: each one lost to the app, if this
            // went on ahead of it.
            for older in self.streams.range_mut(self.head..index).map(|(_, s)| s) {
                if older.end.is_none() && !older.overtaken {
                    older.overtaken = true;
                    self.stats.overtaken += 1;
                }
            }
        }
        self.advance(now, on);
    }

    /// The head on past the streams that are over, each new head's waiting
    /// messages handed on.
    fn advance<F: FnMut(Bytes)>(&mut self, now: std::time::Instant, on: &mut F) {
        while self.streams.get(&self.head).is_some_and(|s| s.end.is_some()) {
            self.streams.remove(&self.head);
            self.head += 1;
            self.release(now, on);
        }
    }

    /// The head's waiting messages handed on.
    fn release<F: FnMut(Bytes)>(&mut self, now: std::time::Instant, on: &mut F) {
        let Some(head) = self.streams.get_mut(&self.head) else { return };
        if let Some(since) = head.since.take() {
            self.stats.held += 1;
            self.held_most = self.held_most.max(now.saturating_duration_since(since));
        }
        for message in head.messages.drain(..) {
            self.held -= message.len();
            on(message);
        }
    }

    /// Once a message waited LATE, or too many bytes wait: the head given
    /// up, and the next, until none is so. When to look again (the oldest
    /// waiting message's LATE), if any waits.
    fn tick<F: FnMut(Bytes)>(&mut self, now: std::time::Instant, on: &mut F) -> Option<std::time::Instant> {
        loop {
            let oldest = self.streams.values().filter_map(|s| s.since).min()?;
            if now.saturating_duration_since(oldest) < LATE && self.held <= HELD_MOST {
                return Some(oldest + LATE);
            }
            // Its stop dropped with it: its reader stops it.
            if self.streams.remove(&self.head).is_none_or(|head| head.end.is_none()) {
                self.stats.given_up += 1;
            }
            self.head += 1;
            self.release(now, on);
            self.advance(now, on);
        }
    }

    /// The last second's line, if it is over and groups came.
    fn line(&mut self, seconds: &mut Seconds<Received>, now: std::time::Instant) -> Option<String> {
        let (most, bytes) = (self.held_most, self.held_bytes_most);
        let line = seconds.note(now, self.stats, |stats, before, over| stats.line(before, over, most, bytes));
        if line.is_some() {
            (self.held_most, self.held_bytes_most) = (Duration::ZERO, self.held);
        }
        line
    }
}

/// The other side's groups: each stream read by a task of its own as it
/// comes, their messages handed to on in the streams' order (Order: one
/// task calls it; on the runtime, it must not block), until the connection
/// is over. A stream reset ends its reader at once (its credit back to the
/// other side). A control stream's frames to control, if we take one (both
/// hellos said so), else it is stopped.
pub(crate) async fn read<F, C>(conn: Connection, on: Arc<F>, control: Option<Arc<C>>)
where F: Fn(Bytes) + Send + Sync + 'static, C: Fn(u8, u16, Vec<u8>) + Send + Sync + 'static {
    let (reads, read) = mpsc::unbounded_channel();
    tokio::spawn(order(read, on, conn.remote_address()));
    while let Ok(stream) = conn.accept_uni().await {
        let index = stream.id().index();
        let (stop, stopped) = oneshot::channel();
        if reads.send(Read::Taken(index, stop)).is_err() {
            return;
        }
        tokio::spawn(read_stream(stream, index, reads.clone(), stopped, control.clone()));
    }
}

/// The streams' messages to on in order (Order), until no reader is left;
/// its counts said once a second while groups come.
async fn order<F: Fn(Bytes)>(mut reads: mpsc::UnboundedReceiver<Read>, on: Arc<F>, remote: SocketAddr) {
    let mut order = Order::default();
    let mut seconds = Seconds::new(std::time::Instant::now(), order.stats);
    let mut wake: Option<std::time::Instant> = None;
    let mut deliver = |message: Bytes| on(message);
    loop {
        let at = wake.into_iter().chain(seconds.due(&order.stats)).min().map(Instant::from_std);
        let read = tokio::select! {
            read = reads.recv() => match read {
                Some(read) => Some(read),
                None => break,
            },
            _ = sleep_until(at) => None,
        };
        let now = std::time::Instant::now();
        match read {
            Some(Read::Taken(index, stop)) => order.taken(index, stop),
            Some(Read::Message(index, message)) => order.message(index, message, now, &mut deliver),
            Some(Read::End(index, end)) => order.end(index, end, now, &mut deliver),
            None => {}
        }
        wake = order.tick(now, &mut deliver);
        if let Some(line) = order.line(&mut seconds, now) {
            log!("{remote}: {line}");
        }
    }
}

/// A stream's messages to the orderer as they come, then how it ended; it
/// stopped once the orderer gives it up. A control stream's frames to
/// control instead (never given up: its first bytes are read before that
/// is looked at).
async fn read_stream<C>(mut stream: RecvStream, index: u64, reads: mpsc::UnboundedSender<Read>,
                        mut stop: oneshot::Receiver<()>, control: Option<Arc<C>>)
where C: Fn(u8, u16, Vec<u8>) + Send + Sync + 'static {
    let mut head = [0u8; LENGTH];
    let first = stream.read_exact(&mut head).await;
    if first.is_ok() && head == CONTROL_MARK {
        let _ = reads.send(Read::End(index, End::Control));
        if let Some(control) = control {
            read_control(&mut stream, &*control).await;
        }
        let _ = stream.stop(VarInt::from_u32(0));
        return;
    }
    let mut first = Some(first.map(|()| head));
    loop {
        let next = tokio::select! {
            biased;
            _ = &mut stop => None,
            next = next_message(&mut stream, first.take()) => Some(next),
        };
        let Some(next) = next else {
            let _ = stream.stop(VarInt::from_u32(0));
            return;
        };
        match next {
            Ok(message) => {
                if reads.send(Read::Message(index, message)).is_err() {
                    return;
                }
            }
            Err(end) => {
                if end == End::Cut {
                    let _ = stream.stop(VarInt::from_u32(0));
                }
                let _ = reads.send(Read::End(index, end));
                return;
            }
        }
    }
}

/// A group stream's next message (head: its length read already), or how
/// the stream ended: its end at a message's (Finished), else Cut (reset,
/// broken, or more than a sealed message).
async fn next_message(stream: &mut RecvStream, head: Option<Result<[u8; LENGTH], quinn::ReadExactError>>)
                      -> Result<Bytes, End> {
    let head = match head {
        Some(head) => head,
        None => {
            let mut head = [0u8; LENGTH];
            stream.read_exact(&mut head).await.map(|()| head)
        }
    };
    let length = match head {
        Ok(head) => u16::from_be_bytes(head) as usize,
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Err(End::Finished),
        Err(_) => return Err(End::Cut),
    };
    if length > MAX_SEALED {
        return Err(End::Cut);
    }
    let mut sealed = vec![0u8; length];
    stream.read_exact(&mut sealed).await.map_err(|_| End::Cut)?;
    Ok(Bytes::from(sealed))
}

/// A control stream's frames (after its mark), each to control as it comes,
/// until it ends or one is not a frame.
async fn read_control<C: Fn(u8, u16, Vec<u8>)>(stream: &mut quinn::RecvStream, control: &C) {
    let mut decoder = crate::frame::Decoder::default();
    let mut buf = vec![0u8; 16 << 10];
    while let Ok(Some(n)) = stream.read(&mut buf).await {
        decoder.push(&buf[..n]);
        loop {
            match decoder.next() {
                Ok(Some((kind, channel, body))) => control(kind, channel, body),
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant as At;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn bytes(text: &str) -> Bytes {
        Bytes::copy_from_slice(text.as_bytes())
    }

    /// An Order with streams 0..n taken, and their stops' other ends.
    fn taken(n: u64) -> (Order, Vec<oneshot::Receiver<()>>) {
        let mut order = Order::default();
        let stops = (0..n).map(|i| {
            let (stop, stopped) = oneshot::channel();
            order.taken(i, stop);
            stopped
        }).collect();
        (order, stops)
    }

    #[test]
    fn a_newer_streams_messages_wait_until_every_older_one_is_over() {
        let ((mut order, _stops), t) = (taken(3), At::now());
        let mut got = Vec::new();
        order.message(0, bytes("a1"), t, &mut |m| got.push(m));
        // The newest whole first, the middle one in part: both older ones
        // would have been lost to the app.
        order.message(2, bytes("c1"), t, &mut |m| got.push(m));
        order.end(2, End::Finished, t, &mut |m| got.push(m));
        order.message(1, bytes("b1"), t + ms(2), &mut |m| got.push(m));
        order.end(1, End::Finished, t + ms(5), &mut |m| got.push(m));
        assert_eq!(got, [bytes("a1")]);
        assert_eq!(order.tick(t + ms(5), &mut |m| got.push(m)), Some(t + LATE));
        order.message(0, bytes("a2"), t + ms(10), &mut |m| got.push(m));
        order.end(0, End::Finished, t + ms(10), &mut |m| got.push(m));
        assert_eq!(got, ["a1", "a2", "b1", "c1"].map(bytes));
        assert_eq!(order.tick(t + ms(10), &mut |m| got.push(m)), None);
        assert_eq!(order.stats, Received { groups: 3, overtaken: 2, held: 2, ..Received::default() });
        assert_eq!((order.held, order.head, order.held_most), (0, 3, ms(10)));
        // The next stream's, at once.
        order.message(3, bytes("d1"), t + ms(11), &mut |m| got.push(m));
        assert_eq!(got.last(), Some(&bytes("d1")));
    }

    #[test]
    fn a_message_waits_late_at_most_then_the_older_streams_are_given_up() {
        let ((mut order, mut stops), t) = (taken(3), At::now());
        let mut got = Vec::new();
        order.message(0, bytes("a1"), t, &mut |m| got.push(m));
        order.message(2, bytes("c1"), t + ms(1), &mut |m| got.push(m));
        order.end(2, End::Finished, t + ms(1), &mut |m| got.push(m));
        assert_eq!(order.tick(t + LATE, &mut |m| got.push(m)), Some(t + ms(1) + LATE));
        assert_eq!(got, [bytes("a1")]);
        assert!(stops[0].try_recv().is_err_and(|e| e == oneshot::error::TryRecvError::Empty));
        // LATE: both older ones given up (the middle one had nothing yet),
        // their readers told to stop them.
        assert_eq!(order.tick(t + ms(1) + LATE, &mut |m| got.push(m)), None);
        assert_eq!(got, ["a1", "c1"].map(bytes));
        for stopped in &mut stops[..2] {
            assert_eq!(stopped.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        }
        // What still comes on them: dropped.
        order.message(0, bytes("a2"), t + ms(150), &mut |m| got.push(m));
        order.end(1, End::Finished, t + ms(150), &mut |m| got.push(m));
        assert_eq!(got.len(), 2);
        assert_eq!(order.stats, Received { groups: 1, overtaken: 2, held: 1, given_up: 2, dropped: 1, reset: 0 });
    }

    #[test]
    fn a_reset_stream_or_the_control_stream_holds_up_nothing() {
        let ((mut order, _stops), t) = (taken(3), At::now());
        let mut got = Vec::new();
        // The control stream first, its mark not read yet.
        order.message(1, bytes("b1"), t, &mut |m| got.push(m));
        order.end(1, End::Finished, t, &mut |m| got.push(m));
        assert!(got.is_empty());
        order.end(0, End::Control, t + ms(1), &mut |m| got.push(m));
        assert_eq!(got, [bytes("b1")]);
        // A group reset part way (late there): the next one's at once.
        order.message(2, bytes("c1"), t + ms(2), &mut |m| got.push(m));
        let (stop, _stopped) = oneshot::channel();
        order.taken(3, stop);
        order.message(3, bytes("d1"), t + ms(3), &mut |m| got.push(m));
        order.end(2, End::Cut, t + ms(4), &mut |m| got.push(m));
        assert_eq!(got, ["b1", "c1", "d1"].map(bytes));
        assert_eq!((order.stats.groups, order.stats.reset, order.stats.given_up), (1, 1, 0));
    }

    #[test]
    fn too_many_bytes_waiting_give_the_head_up() {
        let ((mut order, _stops), t) = (taken(2), At::now());
        let mut got = Vec::new();
        let big = Bytes::from(vec![7u8; HELD_MOST / 2]);
        order.message(1, big.clone(), t, &mut |m| got.push(m));
        order.message(1, big.clone(), t, &mut |m| got.push(m));
        assert_eq!(order.tick(t, &mut |m| got.push(m)), Some(t + LATE));
        order.message(1, bytes("one more"), t, &mut |m| got.push(m));
        assert_eq!(order.tick(t, &mut |m| got.push(m)), None);
        assert_eq!((got.len(), order.held, order.stats.given_up), (3, 0, 1));
    }

    #[test]
    fn the_groups_sent_are_said_once_a_second_while_they_go() {
        let t = At::now();
        let mut seconds = Seconds::new(t, Stats::default());
        let mut stats = Stats { groups: 30, late: 1, late_bytes: 9000, resets: 1, stream_bytes: 600_000,
                                lost_bytes: 1500, ..Stats::default() };
        assert_eq!(seconds.note(t + ms(500), stats, Stats::line), None);
        assert_eq!(seconds.due(&stats), Some(t + SECOND));
        (stats.groups, stats.superseded, stats.resets, stats.late_unsent) = (60, 1, 2, 1);
        assert_eq!(seconds.note(t + SECOND, stats, Stats::line).as_deref(),
                   Some("groups over 1.0 s: 60 begun, 1 late (1 unsent), 1 superseded, 0 given up there, 2 reset; \
                         0 messages dropped, 0 credit waits, 0 window full; 600 KB on streams, 1.5 KB lost"));
        // Nothing moved: nothing said, nothing to wake for.
        assert_eq!(seconds.due(&stats), None);
        assert_eq!(seconds.note(t + ms(3000), stats, Stats::line), None);
        // The newest's bytes alone: no line.
        stats.stream_bytes += 1000;
        assert_eq!(seconds.note(t + ms(3500), stats, Stats::line), None);
        assert_eq!(seconds.note(t + ms(4100), stats, Stats::line), None);
        stats.stopped += 1;
        assert_eq!(seconds.note(t + ms(4600), stats, Stats::line), None);
        assert!(seconds.note(t + ms(5200), stats, Stats::line).unwrap().contains(" 1 given up there"));
    }

    #[test]
    fn the_groups_come_are_said_once_a_second_while_they_come() {
        let ((mut order, _stops), t) = (taken(3), At::now());
        let mut seconds = Seconds::new(t, order.stats);
        order.message(1, bytes("b1"), t, &mut |_| {});
        order.end(1, End::Finished, t, &mut |_| {});
        assert_eq!(order.line(&mut seconds, t + ms(10)), None);
        order.end(0, End::Finished, t + ms(12), &mut |_| {});
        order.end(2, End::Cut, t + ms(20), &mut |_| {});
        assert_eq!(order.line(&mut seconds, t + SECOND).as_deref(),
                   Some("groups in over 1.0 s: 2 whole, 1 reset; 1 not whole when a newer one was, \
                         1 held for an older one (12 ms, 0 KB at most), 0 given up, 0 messages dropped after"));
        assert_eq!((order.held_most, seconds.due(&order.stats)), (Duration::ZERO, None));
    }
}
