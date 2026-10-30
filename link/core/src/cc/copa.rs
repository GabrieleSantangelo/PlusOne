//! Copa (Arun and Balakrishnan, NSDI 2018), the delay-based controller
//! mvfst uses for live video, from the Stage 1 spike (link/quic-spike
//! src/copa.rs; δ = 0.1 kept the realtime best on a slow home Wi-Fi): the
//! window follows a target rate of 1 / (δ × queuing delay) packets a
//! second, so the queue it keeps standing is about 2.5 / δ packets,
//! whatever the path's speed. The queuing delay: the least round trip of
//! the last half round trip (standing) over the least of 10 s. Loss alone
//! does not cut it (Wi-Fi loses packets with no queue at all); persistent
//! congestion does. Growing only while the application sends all it may:
//! no probing while idle. As the spike's, but its round trips kept bounded
//! (the least of each 50 ms over 10 s; the standing one from a sliding
//! minimum), not a sample a packet.

use super::shared::Report;
use quinn::congestion::Controller;
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The least round trip is the least of this long, kept as the least of
/// each BUCKET.
const LEAST_OVER: Duration = Duration::from_secs(10);
const BUCKET: Duration = Duration::from_millis(50);
const MIN_PACKETS: u64 = 4;
const VELOCITY_MAX: u64 = 64;

/// Copa's tunable: δ, the queue it keeps standing about 2.5 / δ packets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CopaParams {
    pub delta: f64,
}

impl Default for CopaParams {
    fn default() -> CopaParams {
        CopaParams { delta: 0.1 }
    }
}

impl CopaParams {
    pub(crate) fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "delta" => self.delta = super::select::number(value, 0.01, 2.0)?,
            _ => return Err("unknown".into()),
        }
        Ok(())
    }

    pub(crate) fn describe(&self) -> String {
        format!("delta={}", self.delta)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Copa {
    delta: f64,
    mtu: u64,
    window: u64,
    slow_start: bool,
    /// The least round trip of each BUCKET over LEAST_OVER (its start, the
    /// least), and the least of them.
    buckets: VecDeque<(Instant, Duration)>,
    least: Option<Duration>,
    /// The round trips of the last half round trip, as a sliding minimum
    /// (each less than those after it): the first is the standing one.
    recent: VecDeque<(Instant, Duration)>,
    /// The last queuing delay seen, and which way it went (the trace's).
    queue: Duration,
    up: bool,
    velocity: u64,
    /// Up (true) or down this round trip, and how many round trips in a row.
    direction: Option<bool>,
    same_direction: u32,
    round_start: Instant,
    round_window: u64,
}

impl Copa {
    pub(crate) fn new(now: Instant, mtu: u16, params: CopaParams) -> Copa {
        let mtu = mtu as u64;
        Copa { delta: params.delta, mtu, window: 10 * mtu, slow_start: true, buckets: VecDeque::new(), least: None,
               recent: VecDeque::new(), queue: Duration::ZERO, up: true, velocity: 1, direction: None,
               same_direction: 0, round_start: now, round_window: 10 * mtu }
    }

    /// A packet sent at sent, of bytes, acknowledged now; srtt: quinn's
    /// smoothed round trip.
    pub(crate) fn ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, srtt: Duration) {
        let sample = now.saturating_duration_since(sent);
        match self.buckets.back_mut() {
            Some((start, least)) if now.saturating_duration_since(*start) < BUCKET => *least = (*least).min(sample),
            _ => self.buckets.push_back((now, sample)),
        }
        let mut gone = false;
        while self.buckets.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > LEAST_OVER) {
            self.buckets.pop_front();
            gone = true;
        }
        self.least = if gone {
            self.buckets.iter().map(|(_, rtt)| *rtt).min()
        } else {
            Some(self.least.map_or(sample, |least| least.min(sample)))
        };
        while self.recent.back().is_some_and(|(_, rtt)| *rtt >= sample) {
            self.recent.pop_back();
        }
        self.recent.push_back((now, sample));
        let since = now.checked_sub(srtt / 2).unwrap_or(now);
        while self.recent.len() > 1 && self.recent.front().is_some_and(|(at, _)| *at < since) {
            self.recent.pop_front();
        }
        let (Some(least), Some(&(_, standing))) = (self.least, self.recent.front()) else { return };
        self.queue = standing.saturating_sub(least);
        let queue = self.queue.as_secs_f64();
        // Packets a second: the target, from the queue; the current, from the window.
        let target = if queue > 0.0 { 1.0 / (self.delta * queue) } else { f64::INFINITY };
        let current = self.window as f64 / self.mtu as f64 / standing.as_secs_f64().max(1e-4);
        self.up = current <= target;

        // Each round trip: the velocity doubles after three the same way.
        if now.saturating_duration_since(self.round_start) >= srtt {
            let went_up = self.window > self.round_window;
            match self.direction {
                Some(d) if d == went_up => {
                    self.same_direction += 1;
                    if self.same_direction >= 3 {
                        self.velocity = (self.velocity * 2).min(VELOCITY_MAX);
                    }
                }
                _ => {
                    self.same_direction = 0;
                    self.velocity = 1;
                }
            }
            self.direction = Some(went_up);
            self.round_start = now;
            self.round_window = self.window;
        }

        if self.slow_start {
            if self.up {
                if !app_limited {
                    self.window += bytes;
                }
                return;
            }
            self.slow_start = false;
        }
        let change = (bytes as f64 * self.mtu as f64 * self.velocity as f64 / (self.delta * self.window as f64)) as u64;
        if self.up {
            if !app_limited {
                self.window += change;
            }
        } else {
            self.window = self.window.saturating_sub(change).max(MIN_PACKETS * self.mtu);
        }
    }
}

impl Controller for Copa {
    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.ack(now, sent, bytes, app_limited, rtt.get());
    }

    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, persistent: bool, _lost_bytes: u64) {
        if persistent {
            self.window = MIN_PACKETS * self.mtu;
            self.velocity = 1;
        }
    }

    fn on_mtu_update(&mut self, mtu: u16) {
        self.mtu = mtu as u64;
        self.window = self.window.max(MIN_PACKETS * self.mtu);
    }

    fn window(&self) -> u64 {
        self.window
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

impl Report for Copa {
    /// The window a smoothed round trip (quinn paces at 1.25 times it).
    fn budget(&self, srtt: Duration) -> f64 {
        self.window as f64 / srtt.as_secs_f64().max(0.001)
    }

    fn state(&self) -> String {
        format!("Copa {}{}", if self.up { "Up" } else { "Down" }, if self.slow_start { " (slow start)" } else { "" })
    }

    fn queue(&self) -> Option<Duration> {
        Some(self.queue)
    }

    /// Its velocity, and δ.
    fn trend(&self) -> (f64, f64) {
        (self.velocity as f64, self.delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copa through a path of capacity (bytes a second) and round trip base
    /// (plus jitter(ms into it)) for ms: what its window holds past the
    /// path's bandwidth-delay product waits in the path's queue, and is
    /// acknowledged at the path's rate.
    fn path(copa: &mut Copa, now: &mut Instant, ms: u64, capacity: f64, base: Duration, app_limited: bool,
            jitter: impl Fn(u64) -> Duration) {
        let (mut srtt, mut credit) = (base, 0.0);
        for i in 0..ms {
            *now += Duration::from_millis(1);
            let queued = (copa.window as f64 - capacity * base.as_secs_f64()).max(0.0);
            let sample = base + jitter(i) + Duration::from_secs_f64(queued / capacity);
            srtt = (srtt * 7 + sample) / 8;
            credit += (copa.window as f64 / sample.as_secs_f64()).min(capacity) * 0.001;
            while credit >= 1452.0 {
                credit -= 1452.0;
                copa.ack(*now, *now - sample, 1452, app_limited, srtt);
            }
        }
    }

    fn clean(_: u64) -> Duration {
        Duration::ZERO
    }

    #[test]
    fn a_clean_path_takes_it_up_to_the_paths_rate() {
        let mut now = Instant::now();
        let mut copa = Copa::new(now, 1452, CopaParams::default());
        let before = copa.window;
        path(&mut copa, &mut now, 2000, 5e6, Duration::from_millis(10), false, clean);
        assert!(copa.window > 3 * before, "{} after {before}", copa.window);
        let rate = copa.budget(Duration::from_millis(12));
        assert!(rate > 3e6 && rate < 1e7, "{rate}");
    }

    #[test]
    fn a_queue_takes_it_down_and_holds_it_short() {
        let mut now = Instant::now();
        let mut copa = Copa::new(now, 1452, CopaParams::default());
        path(&mut copa, &mut now, 2000, 5e6, Duration::from_millis(10), false, clean);
        let before = copa.window;
        // The path's rate to a fifth: its queue grows, Copa's window comes down.
        path(&mut copa, &mut now, 3000, 1e6, Duration::from_millis(10), false, clean);
        assert!(!copa.slow_start);
        assert!(copa.window < before * 6 / 10, "{} after {before}", copa.window);
        assert!(copa.window >= MIN_PACKETS * 1452);
        // 1 / (δ × queue) packets a second at the path's 690: about 15 ms
        // queued (oscillating around it).
        assert!(copa.queue < Duration::from_millis(50), "{:?}", copa.queue);
    }

    #[test]
    fn a_loss_is_no_cut_persistent_congestion_is() {
        let mut now = Instant::now();
        let mut copa = Copa::new(now, 1452, CopaParams::default());
        path(&mut copa, &mut now, 1000, 5e6, Duration::from_millis(10), false, clean);
        let before = copa.window;
        copa.on_congestion_event(now, now, false, 10 * 1452);
        assert_eq!(copa.window, before);
        copa.on_congestion_event(now, now, true, 10 * 1452);
        assert_eq!(copa.window, MIN_PACKETS * 1452);
    }

    #[test]
    fn sending_less_than_allowed_is_no_probe() {
        let mut now = Instant::now();
        let mut copa = Copa::new(now, 1452, CopaParams::default());
        let before = copa.window;
        path(&mut copa, &mut now, 2000, 5e6, Duration::from_millis(10), true, clean);
        assert_eq!(copa.window, before);
    }

    #[test]
    fn its_round_trips_stay_bounded() {
        let mut now = Instant::now();
        let mut copa = Copa::new(now, 1452, CopaParams::default());
        // 12 s of a jittery path.
        path(&mut copa, &mut now, 12_000, 5e6, Duration::from_millis(8), false,
             |i| Duration::from_micros((i * 7919) % 5_000));
        assert!(copa.buckets.len() <= 202, "{}", copa.buckets.len());
        assert!(copa.recent.len() <= 2_000, "{}", copa.recent.len());
        assert!(copa.least.is_some_and(|least| least >= Duration::from_millis(8)));
        assert!(copa.window >= MIN_PACKETS * 1452);
    }
}
