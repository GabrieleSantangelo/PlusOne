//! What a controller other than GCC does as GCC does (Wrapped): the rate
//! shared with the session and the budget told the applications (from the
//! controller's own window or rate, conservatively: what quinn paces at,
//! 1.25 window a round trip, less a fifth), every event in the record, its
//! trace in GCC's format (live.py reads both alike), and a window quinn
//! always paces (it stops pacing past u32::MAX). Its decreases are counted
//! as cuts: each time the budget told goes down (past PUBLISH_CHANGE), by
//! loss if one came in the second before, else "by trend" (its delay or
//! its model). Every packet, datagrams too, goes under its window and
//! quinn's pacer, as under GCC's.

use super::c4::C4;
use super::copa::Copa;
use super::{record, tracing, Budget, Choice, MIN_RATE, PUBLISH_CHANGE, PUBLISH_EVERY, TRACE_EVERY, ACKED_WINDOW};
use quinn::congestion::{BbrConfig, Controller, ControllerFactory, ControllerMetrics, CubicConfig};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a controller other than GCC tells the shared part: the budget the
/// applications follow (the screen's bitrate, the speakers' codec) and the
/// trace's fields.
pub(crate) trait Report {
    /// The rate the application may use now (bytes a second): conservative,
    /// at or under what the controller lets through (its nominal rate, not
    /// a probing peak); srtt: quinn's smoothed round trip.
    fn budget(&self, srtt: Duration) -> f64;
    /// Its state, for the log: letters, spaces and parentheses only (live.py
    /// reads it as [A-Za-z ()]+).
    fn state(&self) -> String;
    /// The queuing delay it estimates (its round trip over its least), if
    /// it has one.
    fn queue(&self) -> Option<Duration>;
    /// Two numbers of its own for the trace's "trend A / B".
    fn trend(&self) -> (f64, f64);
}

/// The controller chosen (not GCC), wrapped.
pub(super) fn build(choice: &Choice, now: Instant, mtu: u16, out: Arc<AtomicU64>, budget: Arc<Budget>)
                    -> Box<dyn Controller> {
    let describe = choice.describe();
    match choice {
        Choice::Copa(params) => Box::new(Wrapped::new(Copa::new(now, mtu, params.clone()), now, mtu, out, budget, &describe)),
        Choice::C4(params) => Box::new(Wrapped::new(C4::new(now, mtu, params.clone()), now, mtu, out, budget, &describe)),
        Choice::Bbr => Box::new(Wrapped::new(Reference::bbr(now, mtu), now, mtu, out, budget, &describe)),
        Choice::Cubic => Box::new(Wrapped::new(Reference::cubic(now, mtu), now, mtu, out, budget, &describe)),
        Choice::Gcc(_) => unreachable!("GCC is built on its own"),
    }
}

#[derive(Clone)]
pub(crate) struct Wrapped<C> {
    inner: C,
    start: Instant,
    mtu: u64,
    /// The budget (bytes a second) now, shared with the session.
    rate: f64,
    out: Arc<AtomicU64>,
    budget: Arc<Budget>,
    published: f64,
    published_at: Instant,
    srtt: Duration,
    least: Duration,
    app_limited: bool,
    /// What was acknowledged over ACKED_WINDOW (when, bytes), and its sum.
    acked: VecDeque<(Instant, u64)>,
    acked_bytes: u64,
    /// Lost over the last second (the trace's), and the last loss's time.
    lost: u64,
    loss_window: Instant,
    lost_at: Option<Instant>,
    /// Its decreases: by trend (its delay, its model), by queue (none of
    /// these has such a rule), by loss.
    cuts: [u32; 3],
    traced: Instant,
    id: u32,
}

impl<C: Controller + Report + Clone + 'static> Wrapped<C> {
    pub(crate) fn new(inner: C, now: Instant, mtu: u16, out: Arc<AtomicU64>, budget: Arc<Budget>, describe: &str)
                      -> Wrapped<C> {
        let id = super::next_id();
        record(format_args!("{id} c {describe}"));
        let srtt = Duration::from_millis(50);
        let rate = inner.budget(srtt).max(MIN_RATE);
        out.store(rate as u64, Ordering::Relaxed);
        Wrapped { inner, start: now, mtu: mtu as u64, rate, out, budget, published: 0.0, published_at: now, srtt,
                  least: Duration::MAX, app_limited: false, acked: VecDeque::new(), acked_bytes: 0, lost: 0,
                  loss_window: now, lost_at: None, cuts: [0; 3], traced: now, id }
    }

    fn acked_rate(&self, now: Instant) -> f64 {
        let Some((first, _)) = self.acked.front() else { return 0.0 };
        self.acked_bytes as f64 / now.duration_since(*first).as_secs_f64().max(0.05)
    }

    /// An acknowledgement's bookkeeping: the record, the round trips, what
    /// was acknowledged.
    fn noted(&mut self, now: Instant, sent: Instant, bytes: u64, srtt: Duration, least: Duration) {
        record(format_args!("{} a {} {} {bytes} {}", self.id, now.duration_since(self.start).as_micros(),
                            now.saturating_duration_since(sent).as_micros(), srtt.as_micros()));
        self.srtt = srtt;
        self.least = least;
        self.acked.push_back((now, bytes));
        self.acked_bytes += bytes;
        while self.acked.front().is_some_and(|(t, _)| now.duration_since(*t) > ACKED_WINDOW) {
            if let Some((_, gone)) = self.acked.pop_front() {
                self.acked_bytes -= gone;
            }
        }
    }

    /// The budget again from the controller: shared, told as it moves past
    /// PUBLISH_CHANGE (down at once, up at most every PUBLISH_EVERY), a
    /// decrease counted; the trace.
    fn update(&mut self, now: Instant) {
        let budget = self.inner.budget(self.srtt);
        self.rate = if budget.is_finite() { budget.max(MIN_RATE) } else { MIN_RATE };
        self.out.store(self.rate as u64, Ordering::Relaxed);
        let moved = (self.rate - self.published).abs() > PUBLISH_CHANGE * self.published;
        let down = self.rate < self.published;
        if moved && (down || now.duration_since(self.published_at) >= PUBLISH_EVERY) {
            if down {
                let by_loss = self.lost_at.is_some_and(|at| now.duration_since(at) <= Duration::from_secs(1));
                self.cuts[if by_loss { 2 } else { 0 }] += 1;
            }
            self.published = self.rate;
            self.published_at = now;
            self.budget.publish(self.rate as u64);
        }
        if now.duration_since(self.loss_window) > Duration::from_secs(1) {
            self.lost = 0;
            self.loss_window = now;
        }
        if tracing() && now.duration_since(self.traced) >= TRACE_EVERY {
            self.traced = now;
            let (a, b) = self.inner.trend();
            let queue = self.inner.queue().unwrap_or(if self.least == Duration::MAX {
                Duration::ZERO
            } else {
                self.srtt.saturating_sub(self.least)
            });
            crate::log!("cc {:.0} ms: rate {:.0} KB/s, acked {:.0} KB/s, trend {a:.2} / {b:.2}, {}{}, queue {} ms, \
                         srtt {:.1} ms, cuts: {} by trend, {} by queue, {} by loss, lost {} B",
                        now.duration_since(self.start).as_secs_f64() * 1000.0, self.rate / 1e3,
                        self.acked_rate(now) / 1e3, self.inner.state(),
                        if self.app_limited { " app-limited" } else { "" }, queue.as_millis(),
                        self.srtt.as_secs_f64() * 1e3, self.cuts[0], self.cuts[1], self.cuts[2], self.lost);
        }
    }

    /// An acknowledgement frame read through (quinn's on_end_acks, less the
    /// controller's own).
    fn ended(&mut self, now: Instant, app_limited: bool) {
        record(format_args!("{} e {} {}", self.id, now.duration_since(self.start).as_micros(), app_limited as u8));
        self.app_limited = app_limited;
        self.update(now);
    }

    fn lost(&mut self, now: Instant, persistent: bool, lost_bytes: u64) {
        record(format_args!("{} l {} {lost_bytes} {}", self.id, now.duration_since(self.start).as_micros(),
                            persistent as u8));
        self.lost += lost_bytes;
        self.lost_at = Some(now);
    }
}

impl<C: Controller + Report + Clone + 'static> Controller for Wrapped<C> {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.noted(now, sent, bytes, rtt.get(), rtt.min());
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest: Option<u64>) {
        self.inner.on_end_acks(now, in_flight, app_limited, largest);
        self.ended(now, app_limited);
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, persistent: bool, lost_bytes: u64) {
        self.lost(now, persistent, lost_bytes);
        self.inner.on_congestion_event(now, sent, persistent, lost_bytes);
        // A loss cuts at once (Cubic's window): the budget with it.
        self.update(now);
    }

    fn on_mtu_update(&mut self, mtu: u16) {
        self.mtu = mtu as u64;
        self.inner.on_mtu_update(mtu);
    }

    fn window(&self) -> u64 {
        // Paced always: quinn paces no window past u32::MAX.
        self.inner.window().clamp(2 * self.mtu, u32::MAX as u64)
    }

    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// quinn's own BBR or Cubic, as references: their budget the window a
/// round trip (BBR: or its pacing rate, the less).
pub(crate) struct Reference {
    inner: Box<dyn Controller>,
    name: &'static str,
}

impl Reference {
    fn bbr(now: Instant, mtu: u16) -> Reference {
        Reference { inner: Arc::new(BbrConfig::default()).build(now, mtu), name: "Bbr" }
    }

    fn cubic(now: Instant, mtu: u16) -> Reference {
        Reference { inner: Arc::new(CubicConfig::default()).build(now, mtu), name: "Cubic" }
    }
}

impl Clone for Reference {
    fn clone(&self) -> Reference {
        Reference { inner: self.inner.clone_box(), name: self.name }
    }
}

impl Controller for Reference {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest: Option<u64>) {
        self.inner.on_end_acks(now, in_flight, app_limited, largest);
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, persistent: bool, lost_bytes: u64) {
        self.inner.on_congestion_event(now, sent, persistent, lost_bytes);
    }

    fn on_mtu_update(&mut self, mtu: u16) {
        self.inner.on_mtu_update(mtu);
    }

    fn window(&self) -> u64 {
        self.inner.window()
    }

    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl Report for Reference {
    fn budget(&self, srtt: Duration) -> f64 {
        let metrics = self.inner.metrics();
        let by_window = metrics.congestion_window as f64 / srtt.as_secs_f64().max(0.001);
        match metrics.pacing_rate {
            Some(bits) if bits > 0 => by_window.min(bits as f64 / 8.0),
            _ => by_window,
        }
    }

    fn state(&self) -> String {
        let metrics = self.inner.metrics();
        let slow = metrics.ssthresh.is_some_and(|threshold| metrics.congestion_window < threshold);
        format!("{}{}", self.name, if slow { " (slow start)" } else { "" })
    }

    fn queue(&self) -> Option<Duration> {
        None
    }

    /// The window (KB), and Cubic's slow start threshold or BBR's pacing
    /// rate (KB/s); 0 for none.
    fn trend(&self) -> (f64, f64) {
        let metrics = self.inner.metrics();
        let second = match (metrics.ssthresh, metrics.pacing_rate) {
            (Some(threshold), _) if threshold != u64::MAX => threshold as f64 / 1e3,
            (_, Some(bits)) => bits as f64 / 8e3,
            _ => 0.0,
        };
        (metrics.congestion_window as f64 / 1e3, second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_cut_by_a_loss_is_told_at_once_as_a_loss() {
        let start = Instant::now();
        let budget: Arc<Budget> = Arc::default();
        let out: Arc<AtomicU64> = Arc::default();
        let mut cubic = Wrapped::new(Reference::cubic(start, 1452), start, 1452, out.clone(), budget.clone(), "cubic");
        let mut now = start + Duration::from_millis(200);
        cubic.on_end_acks(now, 0, false, None);
        let (told, before) = budget.last();
        assert!(told == 1 && before as f64 >= MIN_RATE, "{told} {before}");
        assert_eq!(out.load(Ordering::Relaxed), before);
        // A loss: Cubic's window to 0.7 of it, the budget with it, at once.
        now += Duration::from_millis(5);
        cubic.on_congestion_event(now, now - Duration::from_millis(20), false, 1452);
        let (after, rate) = budget.last();
        assert!(after == 2 && (rate as f64) < 0.75 * before as f64, "{rate} after {before}");
        assert_eq!(cubic.cuts, [0, 0, 1]);
        // Persistent congestion: its least window, the budget its least.
        now += Duration::from_millis(5);
        cubic.on_congestion_event(now, now, true, 20 * 1452);
        assert!(cubic.window() >= 2 * 1452 && cubic.window() < 5 * 1452, "{}", cubic.window());
        assert!(budget.last().1 < rate);
    }

    #[test]
    fn a_window_past_u32_is_still_paced() {
        #[derive(Clone)]
        struct Huge;
        impl Controller for Huge {
            fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
            fn on_mtu_update(&mut self, _: u16) {}
            fn window(&self) -> u64 { u64::MAX / 2 }
            fn clone_box(&self) -> Box<dyn Controller> { Box::new(Huge) }
            fn initial_window(&self) -> u64 { 0 }
            fn into_any(self: Box<Self>) -> Box<dyn Any> { self }
        }
        impl Report for Huge {
            fn budget(&self, _: Duration) -> f64 { f64::INFINITY }
            fn state(&self) -> String { "Huge".into() }
            fn queue(&self) -> Option<Duration> { None }
            fn trend(&self) -> (f64, f64) { (0.0, 0.0) }
        }
        let start = Instant::now();
        let mut huge = Wrapped::new(Huge, start, 1452, Arc::default(), Arc::default(), "huge");
        assert_eq!(huge.window(), u32::MAX as u64);
        huge.on_end_acks(start + Duration::from_millis(200), 0, false, None);
        assert_eq!(huge.rate, MIN_RATE, "a budget of no number is the least");
    }
}
