//! Realtime in groups on QUIC (lib.rs Class::Video, session/grouped.rs):
//! each group (a video's frame) on a unidirectional stream of its own, its
//! messages sealed as the datagrams are, each after its length (2 bytes,
//! big-endian). The datagrams (sound, input) still go first in each packet
//! (quinn writes them before any stream's data); the groups go ahead of the
//! session's stream, the older ahead of the newer (their priorities: what
//! QUIC sends again of a frame goes before the next frame), a key group
//! ahead of all. QUIC sends a group's lost data again, within the
//! controller's rate. A group is late LATE after its first message was
//! handed over (the frame's capture, near enough), but a channel's newest
//! group never is (on a still desktop it is the screen for a long time):
//! once a newer one begins, a late group is reset if not all acknowledged,
//! and at once when a newer key group begins (superseded). What of it is not
//! sent then never is, nor is its lost data sent again, and a late frame
//! holds up no newer one. The other side reads each stream as it comes and
//! hands its messages on as datagrams (read()). (After moq-dev/moq's and
//! moxygen's groups, checked against quinn-proto 0.11.19.)

use crate::quic::LATE;
use bytes::Bytes;
use quinn::{Connection, SendStream, VarInt};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;

/// Group streams the other side may have open towards us at once: at 60
/// frames a second, LATE and a long path's round trip (200 ms) hold about
/// 18; some more than three times that.
pub(crate) const MAX_STREAMS: u32 = 64;
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
/// The session's stream lets a group's message waiting for room in quinn's
/// send window go first, this long at most (quic.rs bridge): the window is
/// one for all streams (quic.rs send_window leaves the groups theirs).
pub(crate) const YIELD: Duration = Duration::from_millis(20);

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
    /// Messages dropped before their stream: the queue full, their group
    /// over before it began, or while they waited.
    pub dropped: u64,
    /// Groups whose stream waited for the other side's credit
    /// (MAX_STREAMS open already).
    pub credit_waits: u64,
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
        write!(f, "groups={} late={} late_kb={:.1} superseded={} superseded_kb={:.1} dropped={} credit_waits={} \
                   stream_kb={:.1} datagram_kb={:.1} lost_kb={:.1} lost_cut_kb={:.1} cuts={}",
               self.groups, self.late, kb(self.late_bytes), self.superseded, kb(self.superseded_bytes), self.dropped,
               self.credit_waits, kb(self.stream_bytes), kb(self.datagram_bytes), kb(self.lost_bytes),
               kb(self.lost_after_cut), self.cuts)
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

/// A connection's groups: the writer (started with the first), what it
/// shares with the session's stream (quic.rs bridge).
#[derive(Default)]
pub(crate) struct Groups {
    writer: OnceLock<mpsc::Sender<Piece>>,
    pub(crate) shared: Arc<Shared>,
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
}

#[derive(Default)]
pub(crate) struct Shared {
    /// A group's message waits for room in quinn's send window (told as it
    /// starts and as it ends): the session's stream waits, YIELD at most.
    pub(crate) waiting: AtomicBool,
    pub(crate) notify: Notify,
    /// Each channel's last HISTORY groups as they began (handed over), the
    /// newest last; told as a group begins.
    begun: Mutex<HashMap<u16, VecDeque<Begun>>>,
    began: Notify,
    /// Bytes written on group streams not all acknowledged yet nor reset
    /// (a stream's count until it is): quinn's send window holds them on
    /// top of the session stream's share (quic.rs send_window), so a
    /// group's message never waits for room the bulk took.
    unacked: AtomicU64,
    /// Bytes handed over, and not written yet.
    handed: AtomicU64,
    queued: AtomicU64,
    groups: AtomicU64,
    late: AtomicU64,
    late_bytes: AtomicU64,
    superseded: AtomicU64,
    superseded_bytes: AtomicU64,
    dropped: AtomicU64,
    credit_waits: AtomicU64,
    stream_bytes: AtomicU64,
}

impl Shared {
    /// Bytes on group streams not all acknowledged yet (unacked).
    pub(crate) fn unacked(&self) -> u64 {
        self.unacked.load(Ordering::Relaxed)
    }

    /// Bytes handed over to groups so far.
    pub(crate) fn handed(&self) -> u64 {
        self.handed.load(Ordering::Relaxed)
    }

    fn wait(&self, on: bool) {
        self.waiting.store(on, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Ours (the rest of Stats is the connection's: quic.rs).
    pub(crate) fn stats(&self) -> Stats {
        let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
        Stats { groups: load(&self.groups), late: load(&self.late), late_bytes: load(&self.late_bytes),
                superseded: load(&self.superseded), superseded_bytes: load(&self.superseded_bytes),
                dropped: load(&self.dropped), credit_waits: load(&self.credit_waits),
                stream_bytes: load(&self.stream_bytes), ..Stats::default() }
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

    /// A group over, its stream (if any) reset: counted.
    fn over(&self, why: Over, stream: Option<&mut SendStream>, bytes: u64) {
        // Reset: what of it is not sent never is, nor is what was lost
        // sent again (an error: all of it acknowledged already).
        if stream.is_some_and(|stream| stream.reset(VarInt::from_u32(0)).is_err()) {
            return;
        }
        let (count, of) = match why {
            Over::Late => (&self.late, &self.late_bytes),
            Over::Superseded => (&self.superseded, &self.superseded_bytes),
        };
        count.fetch_add(1, Ordering::Relaxed);
        of.fetch_add(bytes, Ordering::Relaxed);
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
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
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
    /// finished, kept until all of it is acknowledged, or reset once over.
    fn finish(&mut self, channel: u16, shared: &Arc<Shared>) {
        let Some(mut stream) = self.stream.take() else { return };
        let _ = stream.finish();
        let (group, bytes, shared) = (self.group, self.bytes, shared.clone());
        tokio::spawn(async move {
            let acknowledged = stream.stopped();
            if let Err(why) = until_over(&shared, channel, group, acknowledged).await {
                shared.over(why, Some(&mut stream), bytes);
            }
            shared.unacked.fetch_sub(bytes, Ordering::Relaxed);
        });
    }

    /// Over before its last message: its stream reset (counted), its bytes
    /// no longer held.
    fn over(&mut self, why: Over, shared: &Shared) {
        if let Some(mut stream) = self.stream.take() {
            shared.over(why, Some(&mut stream), self.bytes);
            shared.unacked.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}

/// The writer of a connection's groups (on its runtime), until the
/// connection's Groups is gone: each message to its group's stream, a
/// stream opened for each new group (the one before finished). rate: the
/// controller's (quinn's send window follows it).
pub(crate) async fn write(conn: Connection, mut pieces: mpsc::Receiver<Piece>, shared: Arc<Shared>,
                          rate: Arc<AtomicU64>) {
    let mut writing: HashMap<u16, Writing> = HashMap::new();
    let mut opened = 0u32;
    while let Some(piece) = pieces.recv().await {
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
                    shared.credit_waits.fetch_add(1, Ordering::Relaxed);
                    until_over(&shared, channel, group, opening).await
                }
            };
            let stream = match open {
                Ok(Ok(stream)) => {
                    // A key group ahead of all; the others the older the
                    // higher (frames in order, what is sent again of one
                    // before the next), all ahead of the session's stream (0).
                    opened = opened.wrapping_add(1);
                    let priority = if piece.key { i32::MAX } else { i32::MAX - 1 - (opened & 0x3fff_ffff) as i32 };
                    let _ = stream.set_priority(priority);
                    shared.groups.fetch_add(1, Ordering::Relaxed);
                    Some(stream)
                }
                Ok(Err(_)) => None,  // the connection over
                Err(why) => {
                    shared.over(why, None, 0);
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
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // Room for it in quinn's send window, on top of the session
        // stream's share.
        w.bytes += length;
        let unacked = shared.unacked.fetch_add(length, Ordering::Relaxed) + length;
        conn.set_send_window(crate::quic::send_window(rate.load(Ordering::Relaxed), conn.rtt(), unacked));
        if let Err(why) = write_all(stream, &piece.framed, channel, group, &shared).await {
            w.over(why, &shared);
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        shared.stream_bytes.fetch_add(length, Ordering::Relaxed);
        if piece.last {
            w.finish(channel, &shared);
        }
    }
    for (channel, mut w) in writing {
        w.finish(channel, &shared);
    }
}

/// data on stream, all of it, unless its group is over first (why), or the
/// stream or the connection is (Late). A wait for room in quinn's send
/// window is told (Shared::waiting): the session's stream lets it go first.
async fn write_all(stream: &mut SendStream, mut data: &[u8], channel: u16, group: u32, shared: &Shared)
                   -> Result<(), Over> {
    while !data.is_empty() {
        let at_once = tokio::select! {
            biased;
            written = stream.write(data) => Some(written),
            _ = std::future::ready(()) => None,
        };
        let written = match at_once {
            Some(written) => written,
            None => {
                shared.wait(true);
                let written = until_over(shared, channel, group, stream.write(data)).await;
                shared.wait(false);
                written?
            }
        };
        data = &data[written.map_err(|_| Over::Late)?..];
    }
    Ok(())
}

/// The other side's groups: each stream's messages, in order, to on as they
/// come (on the runtime: on must not block), until the connection is over.
/// A stream reset ends its reader at once (its credit back to the other
/// side).
pub(crate) async fn read<F: Fn(Bytes) + Send + Sync + 'static>(conn: Connection, on: Arc<F>) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let on = on.clone();
        tokio::spawn(async move {
            let mut head = [0u8; LENGTH];
            while stream.read_exact(&mut head).await.is_ok() {
                let length = u16::from_be_bytes(head) as usize;
                if length > MAX_SEALED {
                    let _ = stream.stop(VarInt::from_u32(0));
                    return;
                }
                let mut sealed = vec![0u8; length];
                if stream.read_exact(&mut sealed).await.is_err() {
                    return;
                }
                on(Bytes::from(sealed));
            }
        });
    }
}
