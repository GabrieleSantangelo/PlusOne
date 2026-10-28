//! Copa (Arun and Balakrishnan, NSDI 2018), the delay-based controller
//! mvfst uses for live video, for the spike's realtime: the window follows
//! a target rate of 1 / (delta * queuing delay) packets a second, so the
//! queue it keeps standing is about 2.5 / delta packets, whatever the
//! path's speed. Loss alone does not cut it (Wi-Fi loses packets with no
//! queue at all); persistent congestion does. Growing only while the
//! application sends all it may: no probing while idle (§3.11).

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The least round trip is the least of this long.
const MIN_WINDOW: Duration = Duration::from_secs(10);
const MIN_PACKETS: u64 = 4;
const VELOCITY_MAX: u64 = 64;

#[derive(Debug, Clone)]
pub struct CopaConfig {
    pub delta: f64,
}

impl ControllerFactory for CopaConfig {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        Box::new(Copa::new(now, mtu, self.delta))
    }
}

#[derive(Debug, Clone)]
struct Copa {
    delta: f64,
    mtu: u64,
    window: u64,
    slow_start: bool,
    /// Round-trip samples (when, how long): the least over MIN_WINDOW, and
    /// the "standing" one, the least over the last half smoothed RTT.
    samples: VecDeque<(Instant, Duration)>,
    velocity: u64,
    /// Up (true) or down this round trip, and how many round trips in a row.
    direction: Option<bool>,
    same_direction: u32,
    round_start: Instant,
    round_window: u64,
}

impl Copa {
    fn new(now: Instant, mtu: u16, delta: f64) -> Copa {
        let mtu = mtu as u64;
        Copa { delta, mtu, window: 10 * mtu, slow_start: true, samples: VecDeque::new(), velocity: 1, direction: None,
               same_direction: 0, round_start: now, round_window: 10 * mtu }
    }

    fn least(&self, since: Instant) -> Option<Duration> {
        self.samples.iter().filter(|(at, _)| *at >= since).map(|(_, rtt)| *rtt).min()
    }
}

impl Controller for Copa {
    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        let sample = now.saturating_duration_since(sent);
        self.samples.push_back((now, sample));
        while self.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > MIN_WINDOW) {
            self.samples.pop_front();
        }
        let smoothed = rtt.get();
        let (Some(least), Some(standing)) = (self.least(now - MIN_WINDOW), self.least(now - smoothed / 2)) else {
            return;
        };
        let queue = standing.saturating_sub(least).as_secs_f64();
        // Packets a second: the target, from the queue; the current, from the window.
        let target = if queue > 0.0 { 1.0 / (self.delta * queue) } else { f64::INFINITY };
        let current = self.window as f64 / self.mtu as f64 / standing.as_secs_f64().max(1e-4);
        let up = current <= target;

        // Each round trip: the velocity doubles after three the same way.
        if now.duration_since(self.round_start) >= smoothed {
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
            if up {
                if !app_limited {
                    self.window += bytes;
                }
                return;
            }
            self.slow_start = false;
        }
        let change = (bytes as f64 * self.mtu as f64 * self.velocity as f64 / (self.delta * self.window as f64)) as u64;
        if up {
            if !app_limited {
                self.window += change;
            }
        } else {
            self.window = self.window.saturating_sub(change).max(MIN_PACKETS * self.mtu);
        }
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
