//! A delay-target controller in LEDBAT's family (RFC 6817, with LEDBAT++'s
//! multiplicative decrease), with a target for a second screen: the queue
//! it lets stand is TARGET milliseconds whatever the path's speed (Copa's
//! is a number of packets: 60 ms at 0.5 MB/s). Below the target the window
//! grows in proportion to how far below; above it, it is cut once a round
//! trip in proportion to how far above (at most by half). A loss with no
//! queue (Wi-Fi's own) cuts it a little; persistent congestion to the
//! least. Never grown while the application leaves it unused (§3.11: no
//! probing while idle).

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BASE_WINDOW: Duration = Duration::from_secs(10);
const MIN_PACKETS: u64 = 4;
const LOSS_CUT: f64 = 0.85;

#[derive(Debug, Clone)]
pub struct LedbatConfig {
    pub target: Duration,
    /// Packets a round trip the window may grow by, with no queue at all.
    pub gain: f64,
}

impl ControllerFactory for LedbatConfig {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        let mtu = mtu as u64;
        Box::new(Ledbat { config: (*self).clone(), mtu, window: 10 * mtu, slow_start: true, samples: VecDeque::new(),
                          cut_at: now, standing: None })
    }
}

#[derive(Debug, Clone)]
struct Ledbat {
    config: LedbatConfig,
    mtu: u64,
    window: u64,
    slow_start: bool,
    samples: VecDeque<(Instant, Duration)>,
    /// The last cut: at most one a round trip.
    cut_at: Instant,
    standing: Option<Duration>,
}

impl Ledbat {
    fn least(&self, since: Instant) -> Option<Duration> {
        self.samples.iter().filter(|(at, _)| *at >= since).map(|(_, rtt)| *rtt).min()
    }

    fn cut(&mut self, now: Instant, smoothed: Duration, factor: f64) {
        if now.duration_since(self.cut_at) < smoothed {
            return;
        }
        self.cut_at = now;
        self.slow_start = false;
        self.window = ((self.window as f64 * factor) as u64).max(MIN_PACKETS * self.mtu);
    }
}

impl Controller for Ledbat {
    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.samples.push_back((now, now.saturating_duration_since(sent)));
        while self.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > BASE_WINDOW) {
            self.samples.pop_front();
        }
        let smoothed = rtt.get();
        // The standing round trip: the least of the last one (Wi-Fi's own
        // jitter, an aggregate's wait, is no queue; one that stays is).
        let (Some(base), Some(standing)) = (self.least(now - BASE_WINDOW), self.least(now - smoothed.max(Duration::from_millis(10))))
        else { return };
        self.standing = Some(standing);
        let queue = standing.saturating_sub(base).as_secs_f64();
        let target = self.config.target.as_secs_f64();
        if queue > target {
            // Above: cut, more the further above (LEDBAT++), at most by half.
            let factor = (1.0 - 0.5 * (queue - target) / target).max(0.5);
            self.cut(now, smoothed, factor);
            return;
        }
        if app_limited {
            return;
        }
        if self.slow_start && queue < target / 2.0 {
            self.window += bytes;
            return;
        }
        self.slow_start = false;
        let off = (target - queue) / target;
        self.window += (self.config.gain * off * bytes as f64 * self.mtu as f64 / self.window as f64) as u64;
    }

    fn on_congestion_event(&mut self, now: Instant, _sent: Instant, persistent: bool, _lost_bytes: u64) {
        if persistent {
            self.window = MIN_PACKETS * self.mtu;
            self.slow_start = false;
            return;
        }
        let smoothed = self.standing.unwrap_or(Duration::from_millis(50));
        self.cut(now, smoothed, LOSS_CUT);
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
