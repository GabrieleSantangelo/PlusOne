//! The congestion controller of a QUIC path (architecture §3.11): a
//! delay-gradient one after GCC (draft-ietf-rmcat-gcc-02; WebRTC's
//! TrendlineEstimator, OveruseDetector and AimdRateControl), on the sender
//! alone, from the Stage 1 spike (link/quic-spike, measured on Wi-Fi
//! Direct, a phone's hotspot and a slow home Wi-Fi). The acknowledgements'
//! times stand for the arrivals (the peer acknowledges within 2 ms: quic.rs
//! asks it). Not the queue's size, which on Wi-Fi includes the
//! aggregation's own wait, but whether it grows: the slope of the delay over
//! the last packet groups against a threshold that follows the path's own
//! jitter. Growing: the rate is cut to 0.85 of what was acknowledged (at
//! most 30 %, a round trip apart); steady: up 8 % a second, by a
//! packet a round trip near the last cut, doubling a round trip after 2 s
//! clean with the window full; never past 1.5 times what was acknowledged
//! (no probing while idle). Loss cuts it only past 10 % (Wi-Fi loses
//! packets with no queue). The window is the rate times the round trip
//! (quinn paces at window / rtt); the rate is shared with the session, for
//! realtime late to be dropped and for the encoders to follow.
//!
//! For a live A/B, another controller may run in its place
//! (PLUSONE_LINK_CC, select.rs): this one with the l23 rules, Copa, or
//! quinn's BBR and Cubic; each tells the budget, shares the rate,
//! records and traces as this one does (shared.rs).

mod copa;
mod select;
mod shared;

pub(crate) use select::{selected, Choice};
#[cfg(test)]
pub(crate) use select::NAMES;

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const GROUP: Duration = Duration::from_millis(5);
/// gcc-l23 (GccParams::l23), from the l23 branch (9ae489c): a burst
/// (packets let go together, WebRTC's BelongsToBurst) spans BURST_MOST of
/// arrivals at most. The acknowledgements silent past SILENCE (and past
/// SILENCE_GAPS of their usual gap): the radio away, a background scan's (on
/// a phone's hotspot, every 304 s for 3.8 s, about 20 silences of 130 ms).
/// What is acknowledged in RELEASE after it was let go together, held at
/// the access point or in a radio's queue: one burst, no queue of ours. Read
/// as a delay growing, each scan took 3-4 cuts, the rate to a third (91 of
/// 106 cuts by trend in 144 min of a hotspot's record; none with this). A
/// silence only if the packet acknowledged after it waited through half of
/// it: the network held it. An application sending nothing is none (its
/// next packet acknowledged a round trip after it was sent: 3283 of the
/// 3522 in that record, each read as a release).
const SILENCE_GAPS: u32 = 4;
const RELEASE: Duration = Duration::from_millis(50);
/// gcc-l23: for HELD_FOR after the acknowledgements stopped past HELD (and
/// SILENCE_GAPS usual gaps), what came after held through half of it, a
/// packet sent with its group's last (a batch split by GROUP's end, or over
/// two acknowledgements) joins it, and the rest of its acknowledgement with
/// it: a radio's trickle (a scan; a Wi-Fi's retries, 32-45 ms on the
/// emulated spikes). Always, it chained a transfer's acknowledgements into
/// groups a quarter to a half fewer, the trend on them slower to see a
/// queue: both ways at once, emulated, the other side's controller saw it
/// first and was cut while starting (a hotspot's 4.2 MB/s to 2.8).
const HELD_FOR: Duration = Duration::from_secs(1);
const TREND_WINDOW: usize = 20;
const SMOOTHING: f64 = 0.9;
/// What is acknowledged, measured over this. Over 200 ms a keyframe filled
/// it: on a hotspot's replay the rate allowed was 3.3 times what went
/// through, 2.3 over 500 ms; on the emulated grid a fixed 4 Mbit/s video
/// beside a download went from 65 to 99 % of frames whole.
const ACKED_WINDOW: Duration = Duration::from_millis(500);
/// Clean this long with the window full: probe up fast again.
const PROBE_AFTER: Duration = Duration::from_secs(2);
const MIN_RATE: f64 = 30_000.0;     // bytes a second
const START_RATE: f64 = 1_250_000.0; // 10 Mbit/s
const MIN_PACKETS: u64 = 4;
/// A queue standing this long is cut, whatever its slope (architecture
/// §3.11: an emergency cut past 20 ms of queuing delay). The gradient alone
/// missed a router's deep queue filled slowly by a video just over the
/// path's rate (356 ms on an emulated home Wi-Fi). Standing: the least
/// round trip of the last STANDING, over the least of BASE (Wi-Fi's own
/// jitter is no queue: it does not stand half a second).
const QUEUE_LIMIT: Duration = Duration::from_millis(20);
const STANDING: Duration = Duration::from_millis(500);
const BASE: Duration = Duration::from_secs(10);
/// The round trips kept as the least of each BUCKET (200 over BASE, not a
/// sample a packet: a million at a fast path's rate).
const BUCKET: Duration = Duration::from_millis(50);
/// A queue stands only if packets went through most of STANDING: these
/// buckets of it at least. Idle, a keepalive's round trip every few
/// seconds is all there is, and a Wi-Fi in power saving lengthens it (28
/// to 58 ms on a phone's hotspot): that cut a session doing nothing to the
/// least rate, 17 times, and the screen started again from there.
const STANDING_BUCKETS: usize = 6;
/// The budget told the application (architecture §3.11): when the rate
/// moved past this share of the last told, going up at most every
/// PUBLISH_EVERY, going down at once.
const PUBLISH_CHANGE: f64 = 0.10;
const PUBLISH_EVERY: Duration = Duration::from_millis(100);

/// A QUIC path's controller (the one PLUSONE_LINK_CC chose, else GCC),
/// and what it shares with the session.
#[derive(Debug, Clone)]
pub struct CcConfig {
    /// The rate (bytes a second) for the application to read.
    pub rate: Arc<AtomicU64>,
    pub budget: Arc<Budget>,
    choice: Choice,
}

impl Default for CcConfig {
    fn default() -> CcConfig {
        CcConfig::with(selected().clone())
    }
}

impl CcConfig {
    pub(crate) fn with(choice: Choice) -> CcConfig {
        CcConfig { rate: Arc::default(), budget: Arc::default(), choice }
    }
}

impl ControllerFactory for CcConfig {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        let (rate, budget) = (self.rate.clone(), self.budget.clone());
        match &self.choice {
            Choice::Gcc(params) => Box::new(Gcc::new(now, mtu, rate, budget, params.clone())),
            choice => shared::build(choice, now, mtu, rate, budget),
        }
    }
}

/// GCC's tunables (PLUSONE_LINK_CC_PARAMS): by default today's constants,
/// as they were, and the l23 rules off.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GccParams {
    /// gcc-l23: a radio's silence, and its trickle after, read as one burst.
    pub l23: bool,
    /// The threshold's least and most (ms of trend), as it follows the
    /// trend's own size.
    pub threshold_min: f64,
    pub threshold_max: f64,
    /// The trend: the slope times the deltas (60 at most) times this.
    pub threshold_gain: f64,
    /// How fast the threshold follows the trend up and down (a ms).
    pub k_up: f64,
    pub k_down: f64,
    /// Over the threshold this long before a cut (ms), counted in the delay's
    /// growth (today's) or, overuse_send_time, in send time (WebRTC's).
    pub overuse_ms: f64,
    pub overuse_send_time: bool,
    /// A cut: to this share of what was acknowledged.
    pub beta: f64,
    /// A queue standing past this (ms): cut, whatever its slope.
    pub queue_limit_ms: f64,
    /// Loss past this share of a second's packets: cut by half of it.
    pub loss_cut: f64,
    /// gcc-l23's SILENCE, HELD and BURST_MOST (ms).
    pub silence_ms: f64,
    pub held_ms: f64,
    pub burst_most_ms: f64,
}

impl Default for GccParams {
    fn default() -> GccParams {
        GccParams { l23: false, threshold_min: 6.0, threshold_max: 600.0, threshold_gain: 4.0, k_up: 0.0087,
                    k_down: 0.039, overuse_ms: 20.0, overuse_send_time: false, beta: 0.85,
                    queue_limit_ms: QUEUE_LIMIT.as_millis() as f64,
                    loss_cut: 0.1, silence_ms: 60.0, held_ms: 30.0, burst_most_ms: 100.0 }
    }
}

impl GccParams {
    /// One tunable from its text; Err why not (unknown, not a number, out
    /// of its safe range, or one of gcc-l23's with its rules off).
    pub(crate) fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let l23_only = |l23: bool| if l23 { Ok(()) } else { Err("only with gcc-l23".to_owned()) };
        match key {
            "threshold_min" => self.threshold_min = select::number(value, 1.0, 100.0)?,
            "threshold_max" => self.threshold_max = select::number(value, 10.0, 1000.0)?,
            "threshold_gain" => self.threshold_gain = select::number(value, 0.5, 20.0)?,
            "k_up" => self.k_up = select::number(value, 0.0, 0.1)?,
            "k_down" => self.k_down = select::number(value, 0.0, 0.5)?,
            "overuse_ms" => self.overuse_ms = select::number(value, 1.0, 500.0)?,
            "overuse_send_time" => self.overuse_send_time = select::flag(value)?,
            "beta" => self.beta = select::number(value, 0.5, 0.95)?,
            "queue_limit_ms" => self.queue_limit_ms = select::number(value, 5.0, 500.0)?,
            "loss_cut" => self.loss_cut = select::number(value, 0.01, 0.5)?,
            "silence_ms" => { l23_only(self.l23)?; self.silence_ms = select::number(value, 20.0, 500.0)? }
            "held_ms" => { l23_only(self.l23)?; self.held_ms = select::number(value, 5.0, 200.0)? }
            "burst_most_ms" => { l23_only(self.l23)?; self.burst_most_ms = select::number(value, 20.0, 500.0)? }
            _ => return Err("unknown".into()),
        }
        Ok(())
    }

    /// What does not hold together, set back to its default: why.
    pub(crate) fn check(&mut self) -> Option<String> {
        if self.threshold_min > self.threshold_max {
            let why = format!("threshold_min {} over threshold_max {}: both their defaults", self.threshold_min,
                              self.threshold_max);
            let default = GccParams::default();
            (self.threshold_min, self.threshold_max) = (default.threshold_min, default.threshold_max);
            return Some(why);
        }
        None
    }

    pub(crate) fn describe(&self) -> String {
        let mut text = format!("threshold_min={} threshold_max={} threshold_gain={} k_up={} k_down={} overuse_ms={} \
                                overuse_send_time={} beta={} queue_limit_ms={} loss_cut={}",
                               self.threshold_min, self.threshold_max, self.threshold_gain, self.k_up, self.k_down,
                               self.overuse_ms, self.overuse_send_time as u8, self.beta, self.queue_limit_ms,
                               self.loss_cut);
        if self.l23 {
            text += &format!(" silence_ms={} held_ms={} burst_most_ms={}", self.silence_ms, self.held_ms,
                             self.burst_most_ms);
        }
        text
    }
}

/// ms as a Duration, to the µs (20 ms is 20 ms, as the constants were).
fn millis(ms: f64) -> Duration {
    Duration::from_micros((ms * 1000.0).round() as u64)
}

/// The rate as told the application, as it moves (PUBLISH_CHANGE): waited
/// on (wait()), never polled, so nothing wakes while it holds. The
/// controller runs under quinn's lock: it only notes the rate here, and a
/// thread of the session's tells it on.
#[derive(Debug, Default)]
pub struct Budget {
    /// How many told so far, the last (bytes a second), and over (its path
    /// gone).
    told: Mutex<(u64, u64, bool)>,
    changed: Condvar,
}

impl Budget {
    fn publish(&self, rate: u64) {
        let mut told = self.told.lock().unwrap();
        told.0 += 1;
        told.1 = rate;
        self.changed.notify_all();
    }

    /// How many told so far, and the last (bytes a second), not waiting.
    #[cfg(test)]
    pub(crate) fn last(&self) -> (u64, u64) {
        let told = self.told.lock().unwrap();
        (told.0, told.1)
    }

    /// The path gone: wait() returns None from now on.
    pub fn close(&self) {
        self.told.lock().unwrap().2 = true;
        self.changed.notify_all();
    }

    /// The rate told after the after-th (how many told by then, the rate in
    /// bytes a second), waiting for it; None once closed.
    pub fn wait(&self, after: u64) -> Option<(u64, u64)> {
        let mut told = self.told.lock().unwrap();
        while told.0 == after && !told.2 {
            told = self.changed.wait(told).unwrap();
        }
        (!told.2).then_some((told.0, told.1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Signal {
    Normal,
    Over,
    Under,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Hold,
    Increase,
    Decrease,
}

#[derive(Debug, Clone)]
struct Group {
    first_sent: Instant,
    last_sent: Instant,
    arrived: Instant,
    first_arrived: Instant,
}

#[derive(Debug, Clone)]
struct Gcc {
    /// Its tunables, and those in time: queue_limit_ms, silence_ms, held_ms,
    /// burst_most_ms.
    p: GccParams,
    queue_limit: Duration,
    silence: Duration,
    held: Duration,
    burst_most: Duration,
    start: Instant,
    mtu: u64,
    rate: f64,
    out: Arc<AtomicU64>,
    /// The budget told the application: when, which rate.
    budget: Arc<Budget>,
    published: f64,
    published_at: Instant,
    slow_start: bool,
    state: State,
    // Trendline.
    group: Option<Group>,
    /// gcc-l23: the last acknowledgement's time, the usual gap between two
    /// (smoothed, silences left out), until when what comes is a release
    /// (SILENCE), and until when a packet sent with its group's last joins
    /// it (HELD).
    last_ack: Option<Instant>,
    ack_gap: Duration,
    released_until: Option<Instant>,
    held_until: Option<Instant>,
    previous: Option<Group>,
    accumulated: f64,
    smoothed: f64,
    history: VecDeque<(f64, f64)>,
    deltas: u32,
    // Detector.
    threshold: f64,
    last_update: Option<Instant>,
    over_time: f64,
    over_count: u32,
    previous_trend: f64,
    signal: Signal,
    // Rate control.
    acked: VecDeque<(Instant, u64)>,
    acked_bytes: u64,  // the sum of acked's
    max_mean: Option<f64>,
    max_var: f64,
    last_change: Instant,
    lost: u64,
    acked_since: u64,
    loss_window: Instant,
    srtt: Duration,
    last_over: Instant,
    /// The least round trip of each BUCKET over BASE (its start, the
    /// least): the base and the standing round trips.
    samples: VecDeque<(Instant, Duration)>,
    /// PLUSONE_LINK_CC_TRACE=1: what it does, every TRACE_EVERY in the log;
    /// the cuts so far, by why (the trend, a standing queue, loss).
    traced: Instant,
    cuts: [u32; 3],
    why: usize,
    /// Which controller this is, in a record (each connection has one).
    id: u32,
}

const TRACE_EVERY: Duration = Duration::from_millis(250);

/// Each controller's number, in a record (each connection has one).
fn next_id() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLUSONE_LINK_CC_TRACE").as_deref() == Ok("1"))
}

/// PLUSONE_LINK_CC_RECORD=FILE: every event the controllers get, a line
/// each, to replay a real network's in a test (the controller's number,
/// then "a" acknowledged: µs since
/// the controller began, the packet's age in µs, bytes, smoothed round
/// trip in µs; "e" the acknowledgements read: µs, app-limited; "l" lost:
/// µs, bytes, persistent).
fn record(line: std::fmt::Arguments) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<Option<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>> =
        std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("PLUSONE_LINK_CC_RECORD")?;
        std::fs::File::create(path).ok().map(|f| std::sync::Mutex::new(std::io::BufWriter::new(f)))
    });
    if let Some(file) = file {
        let _ = writeln!(file.lock().unwrap(), "{line}");
    }
}

impl Gcc {
    fn new(now: Instant, mtu: u16, out: Arc<AtomicU64>, budget: Arc<Budget>, p: GccParams) -> Gcc {
        out.store(START_RATE as u64, Ordering::Relaxed);
        let id = next_id();
        record(format_args!("{id} c {} {}", if p.l23 { "gcc-l23" } else { "gcc" }, p.describe()));
        let threshold = 12.5f64.clamp(p.threshold_min, p.threshold_max);
        Gcc { queue_limit: millis(p.queue_limit_ms), silence: millis(p.silence_ms), held: millis(p.held_ms),
              burst_most: millis(p.burst_most_ms), p,
              start: now, mtu: mtu as u64, rate: START_RATE, out, budget, published: 0.0, published_at: now,
              slow_start: true, state: State::Hold, group: None,
              last_ack: None, ack_gap: Duration::ZERO, released_until: None, held_until: None,
              previous: None, accumulated: 0.0, smoothed: 0.0, history: VecDeque::new(), deltas: 0, threshold,
              last_update: None, over_time: -1.0, over_count: 0, previous_trend: 0.0, signal: Signal::Normal,
              acked: VecDeque::new(), acked_bytes: 0, max_mean: None, max_var: 0.4, last_change: now, lost: 0,
              acked_since: 0, loss_window: now, srtt: Duration::from_millis(50), last_over: now, samples: VecDeque::new(),
              traced: now, cuts: [0; 3], why: 0, id }
    }

    fn ms(&self, t: Instant) -> f64 {
        t.duration_since(self.start).as_secs_f64() * 1000.0
    }

    fn acked_rate(&self, now: Instant) -> Option<f64> {
        let first = self.acked.front()?.0;
        let span = now.duration_since(first).as_secs_f64().max(0.05);
        Some(self.acked_bytes as f64 / span)
    }

    /// A group complete: its delay variation into the trendline, the trend
    /// against the threshold.
    fn group_done(&mut self, group: Group) {
        let Some(previous) = self.previous.replace(group.clone()) else { return };
        let arrival_delta = self.ms(group.arrived) - self.ms(previous.arrived);
        let send_delta = self.ms(group.last_sent) - self.ms(previous.last_sent);
        if arrival_delta < 0.0 {
            return; // reordered acknowledgements
        }
        let delta = arrival_delta - send_delta;
        self.deltas = (self.deltas + 1).min(1000);
        self.accumulated += delta;
        self.smoothed = SMOOTHING * self.smoothed + (1.0 - SMOOTHING) * self.accumulated;
        self.history.push_back((self.ms(group.arrived), self.smoothed));
        if self.history.len() > TREND_WINDOW {
            self.history.pop_front();
        }
        if self.history.len() < TREND_WINDOW {
            return;
        }
        // The least-squares slope of the smoothed delay over the arrivals.
        let n = self.history.len() as f64;
        let (mx, my) = self.history.iter().fold((0.0, 0.0), |(x, y), (a, b)| (x + a / n, y + b / n));
        let (num, den) = self.history.iter().fold((0.0, 0.0), |(num, den), (x, y)| {
            (num + (x - mx) * (y - my), den + (x - mx) * (x - mx))
        });
        let slope = if den > 0.0 { num / den } else { 0.0 };
        let trend = (self.deltas.min(60) as f64) * slope * self.p.threshold_gain;
        self.detect(trend, delta, send_delta, group.arrived);
    }

    /// The trend against the threshold: over it for overuse_ms, counted in
    /// the delay's growth (delta) or in send time (send_delta, WebRTC's).
    fn detect(&mut self, trend: f64, delta: f64, send_delta: f64, now: Instant) {
        if trend > self.threshold {
            let grown = if self.p.overuse_send_time { send_delta } else { delta };
            self.over_time = if self.over_time < 0.0 { grown.max(0.0) / 2.0 } else { self.over_time + grown.max(0.0) };
            self.over_count += 1;
            if self.over_time > self.p.overuse_ms && self.over_count > 1 && trend >= self.previous_trend {
                self.over_time = 0.0;
                self.over_count = 0;
                self.signal = Signal::Over;
                self.why = 0;
            }
        } else if trend < -self.threshold {
            self.over_time = -1.0;
            self.over_count = 0;
            self.signal = Signal::Under;
        } else {
            self.over_time = -1.0;
            self.over_count = 0;
            self.signal = Signal::Normal;
        }
        self.previous_trend = trend;
        // The threshold follows the trend's own size (the path's jitter).
        let dt = self.last_update.map(|t| now.duration_since(t).as_secs_f64() * 1000.0).unwrap_or(0.0).min(100.0);
        self.last_update = Some(now);
        if trend.abs() <= self.threshold + 15.0 {
            let k = if trend.abs() < self.threshold { self.p.k_down } else { self.p.k_up };
            self.threshold = (self.threshold + k * (trend.abs() - self.threshold) * dt)
                .clamp(self.p.threshold_min, self.p.threshold_max);
        }
    }

    /// The queue standing now: the least round trip of the last STANDING
    /// over the least of BASE.
    fn standing_queue(&self, now: Instant) -> Duration {
        self.queued(now, STANDING, STANDING_BUCKETS)
    }

    /// The least round trip of the last window over the least of BASE, if
    /// packets went through in so many of its buckets at least (else 0).
    fn queued(&self, now: Instant, window: Duration, buckets: usize) -> Duration {
        let since = now.checked_sub(window).unwrap_or(now);
        if self.samples.iter().rev().take_while(|(at, _)| *at >= since).count() < buckets {
            return Duration::ZERO;
        }
        let least = |since: Instant| self.samples.iter().filter(|(at, _)| *at >= since).map(|(_, rtt)| *rtt).min();
        match (least(since), least(now.checked_sub(BASE).unwrap_or(now))) {
            (Some(standing), Some(base)) => standing.saturating_sub(base),
            _ => Duration::ZERO,
        }
    }

    fn control(&mut self, now: Instant, app_limited: bool) {
        let Some(acked) = self.acked_rate(now) else { return };
        if self.standing_queue(now) > self.queue_limit {
            self.signal = Signal::Over;
            self.why = 1;
        }
        let since = now.duration_since(self.last_change).as_secs_f64();
        // (A cut even while the application sends less than allowed: a
        // video following the rate always does, and with no cut its queue
        // filled the router's, 438 ms, on an emulated home Wi-Fi.)
        if !app_limited && !self.slow_start && now.duration_since(self.last_over) > PROBE_AFTER {
            self.slow_start = true;
        }
        // Another cut only a round trip after the last (10-200 ms), unless
        // what goes through fell under half the rate: one queue, one cut,
        // not a cut at each group while it drains (it went down to the
        // least in a second so).
        if self.signal == Signal::Over {
            // (At least 100 ms: what is acknowledged, over ACKED_WINDOW, shows
            // a cut only that much later; cut again sooner, on it, and the
            // rate fell 19 → 8 → 2 MB/s in three round trips on a hotspot.)
            let wait = self.srtt.clamp(Duration::from_millis(100), Duration::from_millis(200));
            if now.duration_since(self.last_over) < wait && acked >= 0.5 * self.rate {
                self.signal = Signal::Normal;
            }
        }
        match (self.signal, self.state) {
            (Signal::Over, _) => self.state = State::Decrease,
            (Signal::Under, _) => self.state = State::Hold,
            (Signal::Normal, State::Hold) | (Signal::Normal, State::Decrease) => self.state = State::Increase,
            _ => {}
        }
        match self.state {
            State::Decrease => {
                self.cuts[self.why] += 1;
                self.slow_start = false;
                // At most 30 % a cut.
                self.rate = (self.p.beta * acked).max(0.7 * self.rate).max(MIN_RATE).min(self.rate);
                // Where it was cut: its mean and spread (near it, go up slowly).
                let mean = self.max_mean.map(|m| 0.95 * m + 0.05 * acked).unwrap_or(acked);
                let norm = mean.max(1.0);
                self.max_var = (0.95 * self.max_var + 0.05 * (mean - acked).powi(2) / norm).clamp(0.4, 2.5);
                self.max_mean = Some(mean);
                self.state = State::Hold;
                self.signal = Signal::Normal;
                self.last_change = now;
                self.last_over = now;
            }
            // Up, but never past what goes through by more than half (and
            // 10 KB/s): the application may send less than allowed. Never
            // down for it, though (WebRTC's AIMD): the screen follows the
            // rate, so a still image sent little, the rate followed it down,
            // the screen down again, to the least in seconds on a 30 Mbit/s
            // hotspot, with no queue at all.
            State::Increase if !app_limited && self.rate < 1.5 * acked + 10_000.0 => {
                if self.slow_start {
                    // Double a round trip until the first overuse.
                    self.rate *= 2f64.powf(since / self.srtt.as_secs_f64().max(0.001)).min(2.0);
                } else if self.max_mean.is_some_and(|m| (self.rate - m).abs() < 3.0 * (self.max_var * m).sqrt()) {
                    // Near the last cut: a packet a round trip (and 100 ms), or
                    // 5 % a second, the more.
                    let per_second = (self.mtu as f64 / (self.srtt.as_secs_f64() + 0.1)).max(0.05 * self.rate);
                    self.rate += per_second * since.min(1.0);
                } else {
                    self.max_mean = None;
                    self.rate *= 1.08f64.powf(since.min(1.0));
                }
                self.rate = self.rate.min(1.5 * acked + 10_000.0);
                self.last_change = now;
            }
            _ => self.last_change = now,
        }
        self.rate = self.rate.max(MIN_RATE);
    }

    /// An acknowledgement: the packet sent at sent, of bytes, the
    /// smoothed round trip now (quinn's on_ack).
    fn ack(&mut self, now: Instant, sent: Instant, bytes: u64, srtt: Duration) {
        record(format_args!("{} a {} {} {bytes} {}", self.id, now.duration_since(self.start).as_micros(),
                            now.saturating_duration_since(sent).as_micros(), srtt.as_micros()));
        self.srtt = srtt;
        let sample = now.saturating_duration_since(sent);
        match self.samples.back_mut() {
            Some((start, least)) if now.duration_since(*start) < BUCKET => *least = (*least).min(sample),
            _ => self.samples.push_back((now, sample)),
        }
        while self.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > BASE) {
            self.samples.pop_front();
        }
        self.acked.push_back((now, bytes));
        self.acked_bytes += bytes;
        while self.acked.front().is_some_and(|(t, _)| now.duration_since(*t) > ACKED_WINDOW) {
            if let Some((_, gone)) = self.acked.pop_front() {
                self.acked_bytes -= gone;
            }
        }
        self.acked_since += bytes;
        let released = self.p.l23 && self.released(now, sent);
        let (l23, held, burst_most) = (self.p.l23, self.held_until.is_some_and(|until| now < until), self.burst_most);
        match &mut self.group {
            // gcc-l23: let go after a silence (SILENCE), with the group it follows.
            Some(g) if released => {
                g.last_sent = g.last_sent.max(sent);
                g.arrived = now;
            }
            Some(g) if sent.saturating_duration_since(g.first_sent) <= GROUP => {
                g.last_sent = g.last_sent.max(sent);
                g.arrived = now;
            }
            // A burst (WebRTC's BelongsToBurst): it came within GROUP of the
            // group, and sooner after it than it was sent (packets held by
            // the radio, its power saving, an aggregate, let go together):
            // no queue of ours. gcc-l23: or, the radio trickling (HELD),
            // sent with its last (a batch's packets share one send time);
            // BURST_MOST of arrivals at most.
            Some(g) if now.saturating_duration_since(g.arrived) < GROUP
                && if l23 {
                    ((sent <= g.last_sent && held)
                        || now.saturating_duration_since(g.arrived) < sent.saturating_duration_since(g.last_sent))
                        && now.saturating_duration_since(g.first_arrived) < burst_most
                } else {
                    now.saturating_duration_since(g.arrived) < sent.saturating_duration_since(g.last_sent)
                } => {
                g.last_sent = g.last_sent.max(sent);
                g.arrived = now;
            }
            _ => {
                let group = Group { first_sent: sent, last_sent: sent, arrived: now, first_arrived: now };
                let done = self.group.replace(group);
                if let Some(done) = done {
                    self.group_done(done);
                }
            }
        }
    }

    /// gcc-l23: whether the packet sent at sent, acknowledged now, comes in a
    /// release (RELEASE after a silence the network held it through); the
    /// usual gap between acknowledgements, a silence's and a hold's times
    /// kept.
    fn released(&mut self, now: Instant, sent: Instant) -> bool {
        let gap = self.last_ack.replace(now).map(|last| now.saturating_duration_since(last));
        let held = gap.filter(|gap| *gap > self.ack_gap * SILENCE_GAPS && now.saturating_duration_since(sent) > *gap / 2);
        if held.is_some_and(|gap| gap > self.held) {
            self.held_until = Some(now + HELD_FOR);
        }
        match gap {
            Some(gap) if gap > self.silence && held.is_some() => {
                self.released_until = Some(now + RELEASE);
                false
            }
            Some(gap) => {
                // One acknowledgement's packets come at one time (quinn tells
                // them one by one): the gap is between acknowledgements.
                if !gap.is_zero() && gap <= self.silence {
                    self.ack_gap = (self.ack_gap * 7 + gap) / 8;
                }
                self.released_until.is_some_and(|until| now < until)
            }
            None => false,
        }
    }

    /// The rate told the application, if it moved past PUBLISH_CHANGE of the
    /// last told: going down at once (a queue fills meanwhile), going up at
    /// most every PUBLISH_EVERY.
    fn publish(&mut self, now: Instant) {
        let moved = (self.rate - self.published).abs() > PUBLISH_CHANGE * self.published;
        if moved && (self.rate < self.published || now.duration_since(self.published_at) >= PUBLISH_EVERY) {
            self.published = self.rate;
            self.published_at = now;
            self.budget.publish(self.rate as u64);
        }
    }

    /// An acknowledgement frame read through (quinn's on_end_acks).
    fn end_acks(&mut self, now: Instant, app_limited: bool) {
        record(format_args!("{} e {} {}", self.id, now.duration_since(self.start).as_micros(), app_limited as u8));
        // Loss past loss_cut (10 %) over the last second: cut by half of it.
        if now.duration_since(self.loss_window) > Duration::from_secs(1) {
            let total = (self.lost + self.acked_since).max(1);
            let loss = self.lost as f64 / total as f64;
            if loss > self.p.loss_cut {
                self.rate = (self.rate * (1.0 - 0.5 * loss)).max(MIN_RATE);
                self.slow_start = false;
                self.cuts[2] += 1;
            }
            self.lost = 0;
            self.acked_since = 0;
            self.loss_window = now;
        }
        self.control(now, app_limited);
        self.out.store(self.rate as u64, Ordering::Relaxed);
        self.publish(now);
        if tracing() && now.duration_since(self.traced) >= TRACE_EVERY {
            self.traced = now;
            crate::log!("cc {:.0} ms: rate {:.0} KB/s, acked {:.0} KB/s, trend {:.2} / {:.2}, {:?} {:?}{}{}, queue {} ms, \
                         srtt {:.1} ms, cuts: {} by trend, {} by queue, {} by loss, lost {} B",
                        self.ms(now), self.rate / 1e3, self.acked_rate(now).unwrap_or(0.0) / 1e3, self.previous_trend,
                        self.threshold, self.signal, self.state, if self.slow_start { " (slow start)" } else { "" },
                        if app_limited { " app-limited" } else { "" }, self.standing_queue(now).as_millis(),
                        self.srtt.as_secs_f64() * 1e3, self.cuts[0], self.cuts[1], self.cuts[2], self.lost);
        }
    }
}

impl Controller for Gcc {
    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, _app_limited: bool, rtt: &RttEstimator) {
        self.ack(now, sent, bytes, rtt.get());
    }

    fn on_end_acks(&mut self, now: Instant, _in_flight: u64, app_limited: bool, _largest: Option<u64>) {
        self.end_acks(now, app_limited);
    }

    fn on_congestion_event(&mut self, now: Instant, _sent: Instant, persistent: bool, lost_bytes: u64) {
        record(format_args!("{} l {} {lost_bytes} {}", self.id, now.duration_since(self.start).as_micros(),
                            persistent as u8));
        self.lost += lost_bytes;
        if persistent {
            self.rate = MIN_RATE;
            self.slow_start = false;
        }
    }

    fn on_mtu_update(&mut self, mtu: u16) {
        self.mtu = mtu as u64;
    }

    fn window(&self) -> u64 {
        // The rate for a round trip (and a little: quinn's pacer spreads it).
        ((self.rate * (self.srtt.as_secs_f64() + 0.005)) as u64).max(MIN_PACKETS * self.mtu)
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        10 * self.mtu
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// The controller fed timings no network should give (link/fuzz): each
/// 6 bytes an event (time on, a packet's age, bytes); it must never panic,
/// and its rate and threshold stay numbers in their bounds.
#[cfg(fuzzing)]
pub fn fuzz(data: &[u8]) {
    let start = Instant::now();
    let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
    let mut now = start;
    for event in data.chunks_exact(6) {
        now += Duration::from_micros(u16::from_be_bytes([event[0], event[1]]) as u64 * 16);
        let age = Duration::from_micros(u16::from_be_bytes([event[2], event[3]]) as u64 * 8);
        let sent = now.checked_sub(age).unwrap_or(start).max(start);
        let bytes = u16::from_be_bytes([event[4], event[5]]) as u64;
        match event[5] % 8 {
            0 => Controller::on_congestion_event(&mut gcc, now, sent, event[4] & 1 == 1, bytes),
            1 => gcc.end_acks(now, event[4] & 1 == 1),
            _ => gcc.ack(now, sent, bytes, age.max(Duration::from_micros(100))),
        }
        assert!(gcc.rate.is_finite() && gcc.rate >= MIN_RATE, "rate {}", gcc.rate);
        assert!(gcc.threshold.is_finite() && (6.0..=600.0).contains(&gcc.threshold), "threshold {}", gcc.threshold);
        assert!(gcc.window() >= MIN_PACKETS * gcc.mtu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path of rtt, nothing queued: the controller's rate after it
    /// carried bytes_a_second for seconds (acknowledged a packet at a time).
    fn carry(gcc: &mut Gcc, now: &mut Instant, bytes_a_second: f64, seconds: f64, rtt: Duration) {
        carry_with(gcc, now, bytes_a_second, seconds, |_| rtt);
    }

    /// As carry(), each packet's round trip rtt(seconds into it).
    fn carry_with(gcc: &mut Gcc, now: &mut Instant, bytes_a_second: f64, seconds: f64, rtt: impl Fn(f64) -> Duration) {
        let packet = 1200.0;
        let gap = Duration::from_secs_f64(packet / bytes_a_second);
        let (start, end) = (*now, *now + Duration::from_secs_f64(seconds));
        while *now < end {
            *now += gap;
            let rtt = rtt(now.duration_since(start).as_secs_f64());
            gcc.ack(*now, *now - rtt, packet as u64, rtt);
            gcc.end_acks(*now, false);
        }
    }

    struct Rng(u64);

    impl Rng {
        fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + (hi - lo) * ((self.0 >> 11) as f64 / (1u64 << 53) as f64)
        }
    }

    /// A background scan's radio absences (µs from, to), at each of starts
    /// (s), as a phone's hotspot recorded 28 of them (every 303.9 s): off
    /// 130 / on 75 ms; [off, on] [125, 3] [85, 0.5] [75, 20] [60, 20] [60,
    /// 20] [60, 100]; then 14 of off 120-140 / on 60-85 ms: 3.8 s in all.
    fn scans(starts: &[f64]) -> Vec<(u64, u64)> {
        let (mut rng, mut offs) = (Rng(0x9E37_79B9_7F4A_7C15), Vec::new());
        for &s in starts {
            let mut t = s * 1e6;
            let mut cycle = |off: f64, on: f64| {
                offs.push((t as u64, (t + off * 1e3) as u64));
                t += (off + on) * 1e3;
            };
            for (off, on) in [(130.0, 75.0), (125.0, 3.0), (85.0, 0.5), (75.0, 20.0), (60.0, 20.0), (60.0, 20.0),
                              (60.0, 100.0)] {
                cycle(off, on);
            }
            for _ in 0..14 {
                let (off, on) = (rng.uniform(120.0, 140.0), rng.uniform(60.0, 85.0));
                cycle(off, on);
            }
        }
        offs
    }

    /// A desktop through the controller, closed loop, for seconds: a frame
    /// of 44.2 KB each 16.7 ms (and a sound's 200 bytes each 20 ms), what
    /// waits past 100 ms dropped; a sender as quinn's (the window, a pacer
    /// at 1.25 window a round trip in bursts of 2 ms, a frame's packets sent
    /// at once); its radio off as offs says (what it sends waits in its
    /// queue, what is acknowledged at the access point, let go together when
    /// it is back); on, capacity(µs) bytes a second, 4.5 ms each way; the
    /// receiver acknowledging what came 0.3 ms after the last, or 2 ms
    /// after the first. The times of the cuts by trend, and the rate each
    /// 100 µs.
    fn desktop(params: GccParams, offs: &[(u64, u64)], seconds: f64, capacity: &dyn Fn(u64) -> f64)
               -> (Vec<u64>, Vec<(u64, f64)>) {
        let start = Instant::now();
        let at = |us: u64| start + Duration::from_micros(us);
        let mut gcc = Gcc::new(start, 1452, Arc::default(), Arc::default(), params);
        let (dt, one_way, end) = (100u64, 4_500u64, (seconds * 1e6) as u64);
        let mut app: VecDeque<(u64, u64)> = VecDeque::new();
        let mut flight: VecDeque<(u64, u64, u64)> = VecDeque::new();
        let mut radio: VecDeque<(u64, u64)> = VecDeque::new();
        let mut to_receiver: VecDeque<(u64, u64)> = VecDeque::new();
        let mut acks: VecDeque<(u64, Vec<u64>)> = VecDeque::new();
        let mut pending: Vec<u64> = Vec::new();
        let (mut flight_bytes, mut air, mut tokens, mut srtt) = (0u64, 0.0f64, 0.0f64, None::<f64>);
        let (mut pn, mut next_frame, mut next_sound, mut first, mut last, mut off) = (0u64, 0u64, 0u64, 0u64, 0u64, 0);
        let (mut cuts, mut rates, mut trend_cuts) = (Vec::new(), Vec::new(), gcc.cuts[0]);
        let mut t = 0u64;
        while t < end {
            t += dt;
            if t >= next_frame {
                for _ in 0..(44_200 / 1452) {
                    app.push_back((t, 1452));
                }
                next_frame += 16_667;
            }
            if t >= next_sound {
                app.push_back((t, 200));
                next_sound += 20_000;
            }
            while app.front().is_some_and(|(made, _)| t - made > 100_000) {
                app.pop_front();
            }
            let window = gcc.window();
            let pace = 1.25 * window as f64 / srtt.unwrap_or(0.05).max(0.001);
            tokens = (tokens + pace * dt as f64 / 1e6).min((pace * 0.002).max(10.0 * 1452.0));
            let mut blocked = false;
            while let Some(&(_, bytes)) = app.front() {
                if flight_bytes + bytes > window {
                    blocked = true;
                    break;
                }
                if tokens < bytes as f64 {
                    break;
                }
                tokens -= bytes as f64;
                app.pop_front();
                flight.push_back((pn, t, bytes));
                flight_bytes += bytes;
                radio.push_back((pn, bytes));
                pn += 1;
            }
            while off < offs.len() && offs[off].1 <= t {
                off += 1;
            }
            let on = !(off < offs.len() && offs[off].0 <= t);
            if on {
                air += capacity(t) * dt as f64 / 1e6;
                while radio.front().is_some_and(|(_, bytes)| air >= *bytes as f64) {
                    let (packet, bytes) = radio.pop_front().unwrap();
                    air -= bytes as f64;
                    to_receiver.push_back((t + one_way, packet));
                }
                if radio.is_empty() {
                    air = air.min(1452.0);
                }
            }
            while to_receiver.front().is_some_and(|(came, _)| *came <= t) {
                let (_, packet) = to_receiver.pop_front().unwrap();
                if pending.is_empty() {
                    first = t;
                }
                last = t;
                pending.push(packet);
            }
            if !pending.is_empty() && (t - last >= 300 || t - first >= 2_000) {
                acks.push_back((t + one_way, std::mem::take(&mut pending)));
            }
            while on && acks.front().is_some_and(|(came, _)| *came <= t) {
                let (_, packets) = acks.pop_front().unwrap();
                let largest = *packets.iter().max().unwrap();
                if let Some(&(_, sent, _)) = flight.iter().find(|(p, _, _)| *p == largest) {
                    let sample = (t - sent) as f64 / 1e6;
                    srtt = Some(srtt.map_or(sample, |s| 0.875 * s + 0.125 * sample));
                }
                let rtt = Duration::from_secs_f64(srtt.unwrap_or(0.05));
                for packet in packets {
                    if let Some(i) = flight.iter().position(|(p, _, _)| *p == packet) {
                        let (_, sent, bytes) = flight.remove(i).unwrap();
                        flight_bytes -= bytes;
                        gcc.ack(at(t), at(sent), bytes, rtt);
                    }
                }
                gcc.end_acks(at(t), app.is_empty() && !blocked);
            }
            if gcc.cuts[0] != trend_cuts {
                trend_cuts = gcc.cuts[0];
                cuts.push(t);
            }
            rates.push((t, gcc.rate));
        }
        (cuts, rates)
    }

    /// gcc-l23 (its tests on the l23 branch, 9ae489c).
    fn l23() -> GccParams {
        GccParams { l23: true, ..GccParams::default() }
    }

    #[test]
    fn a_wifi_scan_is_no_queue_with_l23() {
        let starts = [20.0, 50.0, 80.0];
        let (cuts, rates) = desktop(l23(), &scans(&starts), 100.0, &|_| 25e6);
        for s in starts {
            let (from, to) = ((s * 1e6) as u64, ((s + 6.0) * 1e6) as u64);
            let during = cuts.iter().filter(|c| (from..to).contains(*c)).count();
            let before = rates.iter().rev().find(|(t, _)| *t < from).unwrap().1;
            let least = rates.iter().filter(|(t, _)| (from..to).contains(t)).map(|(_, r)| *r).fold(f64::MAX, f64::min);
            // Read as a delay growing, each took 3-4 cuts, the rate to 35 %.
            assert!(during <= 1, "the scan at {s} s: {during} cuts by trend");
            assert!(least >= 0.7 * before, "the scan at {s} s: the rate to {:.0} %", 100.0 * least / before);
        }
    }

    #[test]
    fn a_wifi_scan_is_a_queue_without_l23() {
        // The same scans through gcc as it was: cut by trend (the switch
        // changes what it does).
        let starts = [20.0, 50.0, 80.0];
        let (plain, _) = desktop(GccParams::default(), &scans(&starts), 100.0, &|_| 25e6);
        let (with, _) = desktop(l23(), &scans(&starts), 100.0, &|_| 25e6);
        assert!(plain.len() > with.len(), "gcc {} cuts by trend, gcc-l23 {}", plain.len(), with.len());
    }

    #[test]
    fn the_usual_gap_is_between_acknowledgements() {
        // An acknowledgement each 25 ms (a sound's, delayed), two packets
        // each: 70 ms is under four usual gaps, no silence.
        let start = Instant::now();
        let mut gcc = Gcc::new(start, 1452, Arc::default(), Arc::default(), l23());
        let rtt = Duration::from_millis(10);
        let mut t = start;
        for _ in 0..40 {
            t += Duration::from_millis(25);
            for _ in 0..2 {
                gcc.ack(t, t - Duration::from_millis(5), 200, rtt);
            }
            gcc.end_acks(t, true);
        }
        t += Duration::from_millis(70);
        gcc.ack(t, t - Duration::from_millis(5), 200, rtt);
        assert!(gcc.released_until.is_none(), "70 ms read as a silence (usual gap {:?})", gcc.ack_gap);
    }

    #[test]
    fn a_queue_still_cuts_as_soon_with_l23() {
        // The path takes 1.5 MB/s from 20 s, under the desktop's 2.7.
        let (cuts, _) = desktop(l23(), &[], 22.0, &|t| if t >= 20_000_000 { 1.5e6 } else { 12e6 });
        let first = cuts.iter().find(|c| **c >= 20_000_000).map(|c| (c - 20_000_000) / 1_000);
        assert!(first.is_some_and(|ms| ms <= 300), "first cut {first:?} ms after");
    }

    #[test]
    fn a_higher_floor_keeps_the_threshold_up() {
        let start = Instant::now();
        let mut now = start;
        let params = GccParams { threshold_min: 11.0, ..GccParams::default() };
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), params);
        // A clean path: the threshold follows the trend down, to its floor.
        carry(&mut gcc, &mut now, 1e6, 5.0, Duration::from_millis(9));
        assert_eq!(gcc.threshold, 11.0);
        let mut plain = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
        let mut now = start;
        carry(&mut plain, &mut now, 1e6, 5.0, Duration::from_millis(9));
        assert_eq!(plain.threshold, 6.0);
    }

    #[test]
    fn overuse_in_send_time_sees_a_queue_as_soon() {
        // The trend over the threshold counted in send time (WebRTC's): a
        // queue growing is cut no later than with the delay's growth.
        let first_cut = |params: GccParams| {
            let start = Instant::now();
            let mut now = start;
            let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), params);
            carry(&mut gcc, &mut now, 2e6, 2.0, Duration::from_millis(10));
            let mut step = 0;
            while gcc.cuts[0] == 0 && step < 100 {
                step += 1;
                carry(&mut gcc, &mut now, 2e6, 0.01, Duration::from_millis(10 + step / 2));
            }
            step
        };
        let delay = first_cut(GccParams::default());
        let send = first_cut(GccParams { overuse_send_time: true, ..GccParams::default() });
        assert!(delay < 100 && send <= delay, "cut after {send} steps in send time, {delay} in the delay's growth");
    }

    #[test]
    fn sending_less_does_not_take_the_rate_down() {
        let start = Instant::now();
        let mut now = start;
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
        let rtt = Duration::from_millis(10);
        carry(&mut gcc, &mut now, 3e6, 3.0, rtt);
        let before = gcc.rate;
        assert!(before >= 3e6, "{before}");
        // A still image: a tenth of it, for seconds.
        carry(&mut gcc, &mut now, 3e5, 3.0, rtt);
        assert!(gcc.rate >= before, "{} after {before}", gcc.rate);
        assert_eq!(gcc.cuts, [0; 3]);
    }

    #[test]
    fn an_idle_path_in_power_saving_is_no_queue() {
        let start = Instant::now();
        let mut now = start;
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
        carry(&mut gcc, &mut now, 1e6, 2.0, Duration::from_millis(8));
        let before = gcc.rate;
        // Then a keepalive every 2 s, its round trip 30-60 ms (the radio asleep).
        for i in 0..20u64 {
            now += Duration::from_secs(2);
            let rtt = Duration::from_millis(30 + (i * 7) % 30);
            gcc.ack(now, now - rtt, 60, rtt);
            gcc.end_acks(now, true);
        }
        assert_eq!(gcc.cuts[1], 0, "cut by a queue that is none");
        assert!(gcc.rate >= before * 0.99, "{} after {before}", gcc.rate);
    }

    #[test]
    fn a_wifis_jitter_is_no_overuse() {
        let start = Instant::now();
        let mut now = start;
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
        let rtt = Duration::from_millis(9);
        carry(&mut gcc, &mut now, 1e6, 2.0, rtt);
        // Every 300 ms the round trip climbs to 30 ms in 40 ms and comes
        // back in 40 (a hotspot's radio busy elsewhere); its least stays.
        let spike = |t: f64| {
            let into = t % 0.3;
            let up = if into < 0.04 { into / 0.04 } else if into < 0.08 { (0.08 - into) / 0.04 } else { 0.0 };
            Duration::from_secs_f64(0.009 + 0.021 * up)
        };
        carry_with(&mut gcc, &mut now, 1e6, 6.0, spike);
        assert_eq!(gcc.cuts, [0; 3]);
    }

    #[test]
    fn a_queue_growing_is_cut_early() {
        let start = Instant::now();
        let mut now = start;
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
        carry(&mut gcc, &mut now, 2e6, 2.0, Duration::from_millis(10));
        // The path's rate falls under ours: the round trip grows 10 ms each 100 ms.
        for step in 1..=8u64 {
            carry(&mut gcc, &mut now, 2e6, 0.1, Duration::from_millis(10 + 10 * step));
        }
        assert!(gcc.cuts[0] + gcc.cuts[1] > 0, "{:?}", gcc.cuts);
    }

    /// link/target/cc-replay.txt (a record: PLUSONE_LINK_CC_RECORD), its
    /// first controller's, through today's: its rate against what went
    /// through (over a second), and its cuts.
    ///   tools/link-docker.sh test -p plus-one-link --lib -- --ignored replay --nocapture
    #[test]
    #[ignore]
    fn replay() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/cc-replay.txt");
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let start = Instant::now();
        let at = |us: u64| start + Duration::from_micros(us);
        let mut gcc = Gcc::new(start, 1452, Arc::default(), Arc::default(), GccParams::default());
        let mut through: VecDeque<(u64, u64)> = VecDeque::new();
        let (mut through_bytes, mut next, mut over_double, mut seconds) = (0u64, 1_000_000u64, 0u32, 0u32);
        let mut ratios = Vec::new();
        for line in text.lines() {
            let f: Vec<u64> = line.split(' ').filter_map(|w| w.parse().ok()).collect();
            if !line.starts_with("0 ") || f.len() < 2 {
                continue;
            }
            let us = f[1];
            match line.as_bytes()[2] {
                b'a' if f.len() == 5 => {
                    let sent = at(us).checked_sub(Duration::from_micros(f[2])).unwrap_or(start).max(start);
                    gcc.ack(at(us), sent, f[3], Duration::from_micros(f[4]));
                    through.push_back((us, f[3]));
                    through_bytes += f[3];
                    while through.front().is_some_and(|(t, _)| us - t > 1_000_000) {
                        through_bytes -= through.pop_front().unwrap().1;
                    }
                }
                b'e' if f.len() == 3 => {
                    gcc.end_acks(at(us), f[2] == 1);
                    if us >= next {
                        next += 1_000_000;
                        seconds += 1;
                        let went = through_bytes as f64;
                        if went > 10_000.0 {
                            ratios.push(gcc.rate / went);
                            over_double += (gcc.rate > 2.0 * went) as u32;
                        }
                    }
                }
                b'l' if f.len() == 4 => Controller::on_congestion_event(&mut gcc, at(us), at(us), f[3] == 1, f[2]),
                _ => {}
            }
        }
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| ratios.get(((ratios.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
        println!("replay: {seconds} s, rate / what went through (1 s): p10 {:.2}, p50 {:.2}, p90 {:.2}, max {:.2}; \
                  over twice {over_double} s; cuts: {} by trend, {} by queue, {} by loss",
                 q(0.1), q(0.5), q(0.9), q(1.0), gcc.cuts[0], gcc.cuts[1], gcc.cuts[2]);
    }

    #[test]
    fn the_budget_is_told_as_it_moves_a_cut_at_once() {
        let start = Instant::now();
        let mut now = start;
        let budget: Arc<Budget> = Arc::default();
        let mut gcc = Gcc::new(start, 1200, Arc::default(), budget.clone(), GccParams::default());
        carry(&mut gcc, &mut now, 2e6, 3.0, Duration::from_millis(10));
        let (count, rate) = budget.last();
        // Going up: at most one every PUBLISH_EVERY, each past 10 %.
        assert!(count >= 1 && count <= 30, "{count} in 3 s");
        assert!((rate as f64 - gcc.rate).abs() <= 0.1 * rate as f64, "{rate} told, {} now", gcc.rate);
        // A queue: the cut told at once, whatever the last told's time.
        let cuts = gcc.cuts;
        let mut step = 0;
        while gcc.cuts == cuts && step < 50 {
            step += 1;
            carry(&mut gcc, &mut now, 2e6, 0.02, Duration::from_millis(10 + 4 * step));
        }
        assert_ne!(gcc.cuts, cuts, "never cut");
        let (after, told) = budget.last();
        assert!(after > count && told < rate, "the cut not told: {told} after {rate}");
        assert!((told as f64 - gcc.rate).abs() <= 0.1 * told as f64, "{told} told, {} now", gcc.rate);
        budget.close();
        assert_eq!(budget.wait(budget.last().0), None);
    }

    #[test]
    fn a_queue_that_stands_is_cut() {
        let start = Instant::now();
        let mut now = start;
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default(), GccParams::default());
        carry(&mut gcc, &mut now, 2e6, 2.0, Duration::from_millis(10));
        // The same rate, the round trip 40 ms more and staying (a router's queue).
        carry(&mut gcc, &mut now, 2e6, 1.0, Duration::from_millis(50));
        assert!(gcc.cuts[1] > 0);
    }
}
