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
use std::sync::Arc;
use std::time::{Duration, Instant};

const GROUP: Duration = Duration::from_millis(5);
const TREND_WINDOW: usize = 20;
const SMOOTHING: f64 = 0.9;
const THRESHOLD_GAIN: f64 = 4.0;
const OVERUSE_TIME_MS: f64 = 20.0;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const BETA: f64 = 0.85;
const ACKED_WINDOW: Duration = Duration::from_millis(200);
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

#[derive(Debug, Clone)]
pub struct GccConfig {
    /// The rate (bytes a second) for the application to read.
    pub rate: Arc<AtomicU64>,
}

impl ControllerFactory for GccConfig {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        Box::new(Gcc::new(now, mtu, self.rate.clone()))
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
}

impl Gcc {
    fn new(now: Instant, mtu: u16, out: Arc<AtomicU64>) -> Gcc {
        out.store(START_RATE as u64, Ordering::Relaxed);
        Gcc { start: now, mtu: mtu as u64, rate: START_RATE, out, slow_start: true, state: State::Hold, group: None,
              previous: None, accumulated: 0.0, smoothed: 0.0, history: VecDeque::new(), deltas: 0, threshold: 12.5,
              last_update: None, over_time: -1.0, over_count: 0, previous_trend: 0.0, signal: Signal::Normal,
              acked: VecDeque::new(), max_mean: None, max_var: 0.4, last_change: now, lost: 0, acked_since: 0,
              loss_window: now, srtt: Duration::from_millis(50), last_over: now, samples: VecDeque::new() }
    }

    fn ms(&self, t: Instant) -> f64 {
        t.duration_since(self.start).as_secs_f64() * 1000.0
    }

    fn acked_rate(&self, now: Instant) -> Option<f64> {
        let first = self.acked.front()?.0;
        let span = now.duration_since(first).as_secs_f64().max(0.05);
        Some(self.acked.iter().map(|(_, b)| *b).sum::<u64>() as f64 / span)
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
        let least = |since: Instant| self.samples.iter().filter(|(at, _)| *at >= since).map(|(_, rtt)| *rtt).min();
        match (least(now.checked_sub(STANDING).unwrap_or(now)), least(now.checked_sub(BASE).unwrap_or(now))) {
            (Some(standing), Some(base)) => standing.saturating_sub(base),
            _ => Duration::ZERO,
        }
    }

    fn control(&mut self, now: Instant, app_limited: bool) {
        let Some(acked) = self.acked_rate(now) else { return };
        if self.standing_queue(now) > QUEUE_LIMIT {
            self.signal = Signal::Over;
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
            State::Increase if !app_limited => {
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
}

impl Controller for Gcc {
    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, _app_limited: bool, rtt: &RttEstimator) {
        self.srtt = rtt.get();
        let sample = now.saturating_duration_since(sent);
        match self.samples.back_mut() {
            Some((start, least)) if now.duration_since(*start) < BUCKET => *least = (*least).min(sample),
            _ => self.samples.push_back((now, sample)),
        }
        while self.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > BASE) {
            self.samples.pop_front();
        }
        self.acked.push_back((now, bytes));
        while self.acked.front().is_some_and(|(t, _)| now.duration_since(*t) > ACKED_WINDOW) {
            self.acked.pop_front();
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

    fn on_end_acks(&mut self, now: Instant, _in_flight: u64, app_limited: bool, _largest: Option<u64>) {
        // Loss past 10 % over the last second: cut by half of it.
        if now.duration_since(self.loss_window) > Duration::from_secs(1) {
            let total = (self.lost + self.acked_since).max(1);
            let loss = self.lost as f64 / total as f64;
            if loss > 0.1 {
                self.rate = (self.rate * (1.0 - 0.5 * loss)).max(MIN_RATE);
                self.slow_start = false;
            }
            self.lost = 0;
            self.acked_since = 0;
            self.loss_window = now;
        }
        self.control(now, app_limited);
        self.out.store(self.rate as u64, Ordering::Relaxed);
    }

    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, persistent: bool, lost_bytes: u64) {
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
