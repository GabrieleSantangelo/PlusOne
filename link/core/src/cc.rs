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

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const GROUP: Duration = Duration::from_millis(5);
const TREND_WINDOW: usize = 20;
const SMOOTHING: f64 = 0.9;
const THRESHOLD_GAIN: f64 = 4.0;
const OVERUSE_TIME_MS: f64 = 20.0;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const BETA: f64 = 0.85;
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
pub(crate) const PUBLISH_CHANGE: f64 = 0.10;
pub(crate) const PUBLISH_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default)]
pub struct GccConfig {
    /// The rate (bytes a second) for the application to read.
    pub rate: Arc<AtomicU64>,
    pub budget: Arc<Budget>,
}

impl ControllerFactory for GccConfig {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        Box::new(Gcc::new(now, mtu, self.rate.clone(), self.budget.clone()))
    }
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
    fn last(&self) -> (u64, u64) {
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
}

#[derive(Debug, Clone)]
struct Gcc {
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
    fn new(now: Instant, mtu: u16, out: Arc<AtomicU64>, budget: Arc<Budget>) -> Gcc {
        out.store(START_RATE as u64, Ordering::Relaxed);
        Gcc { start: now, mtu: mtu as u64, rate: START_RATE, out, budget, published: 0.0, published_at: now,
              slow_start: true, state: State::Hold, group: None,
              previous: None, accumulated: 0.0, smoothed: 0.0, history: VecDeque::new(), deltas: 0, threshold: 12.5,
              last_update: None, over_time: -1.0, over_count: 0, previous_trend: 0.0, signal: Signal::Normal,
              acked: VecDeque::new(), acked_bytes: 0, max_mean: None, max_var: 0.4, last_change: now, lost: 0,
              acked_since: 0, loss_window: now, srtt: Duration::from_millis(50), last_over: now, samples: VecDeque::new(),
              traced: now, cuts: [0; 3], why: 0,
              id: { static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    NEXT.fetch_add(1, Ordering::Relaxed) } }
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
        let trend = (self.deltas.min(60) as f64) * slope * THRESHOLD_GAIN;
        self.detect(trend, delta, group.arrived);
    }

    fn detect(&mut self, trend: f64, delta: f64, now: Instant) {
        if trend > self.threshold {
            self.over_time = if self.over_time < 0.0 { delta.max(0.0) / 2.0 } else { self.over_time + delta.max(0.0) };
            self.over_count += 1;
            if self.over_time > OVERUSE_TIME_MS && self.over_count > 1 && trend >= self.previous_trend {
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
            let k = if trend.abs() < self.threshold { K_DOWN } else { K_UP };
            self.threshold = (self.threshold + k * (trend.abs() - self.threshold) * dt).clamp(6.0, 600.0);
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
        if self.standing_queue(now) > QUEUE_LIMIT {
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
                self.rate = (BETA * acked).max(0.7 * self.rate).max(MIN_RATE).min(self.rate);
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
        match &mut self.group {
            Some(g) if sent.saturating_duration_since(g.first_sent) <= GROUP => {
                g.last_sent = g.last_sent.max(sent);
                g.arrived = now;
            }
            // A burst (WebRTC's BelongsToBurst): it came within GROUP of the
            // group, and sooner after it than it was sent (packets held by
            // the radio, its power saving, an aggregate, let go together):
            // no queue of ours.
            Some(g) if now.saturating_duration_since(g.arrived) < GROUP
                && now.saturating_duration_since(g.arrived) < sent.saturating_duration_since(g.last_sent) => {
                g.last_sent = g.last_sent.max(sent);
                g.arrived = now;
            }
            _ => {
                let done = self.group.replace(Group { first_sent: sent, last_sent: sent, arrived: now });
                if let Some(done) = done {
                    self.group_done(done);
                }
            }
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
        // Loss past 10 % over the last second: cut by half of it.
        if now.duration_since(self.loss_window) > Duration::from_secs(1) {
            let total = (self.lost + self.acked_since).max(1);
            let loss = self.lost as f64 / total as f64;
            if loss > 0.1 {
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
    let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default());
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

    #[test]
    fn sending_less_does_not_take_the_rate_down() {
        let start = Instant::now();
        let mut now = start;
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default());
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
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default());
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
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default());
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
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default());
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
        let mut gcc = Gcc::new(start, 1452, Arc::default(), Arc::default());
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
        let mut gcc = Gcc::new(start, 1200, Arc::default(), budget.clone());
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
        let mut gcc = Gcc::new(start, 1200, Arc::default(), Arc::default());
        carry(&mut gcc, &mut now, 2e6, 2.0, Duration::from_millis(10));
        // The same rate, the round trip 40 ms more and staying (a router's queue).
        carry(&mut gcc, &mut now, 2e6, 1.0, Duration::from_millis(50));
        assert!(gcc.cuts[1] > 0);
    }
}
