//! Bulk that gives way to realtime on a network whose queues are not ours
//! (a Wi-Fi access point's, a phone's hotspot's): there a big transfer's
//! TCP fills the queue the realtime datagrams wait in too, whatever our
//! own queues let go first. While realtime goes, the round trips of the
//! path's pings (marked as realtime is, so in the same queue) say how much
//! the network has queued beyond the path's least; above TARGET the
//! reliable frames handed to the stream are paced down, below half of it
//! they speed up again, and with no realtime they are not paced at all
//! (a transfer alone goes as fast as the path does). LEDBAT's idea
//! (RFC 6817), with a target for a second screen, not for a download.

use std::time::{Duration, Instant};

/// Delay the network may add to realtime while bulk goes.
const TARGET: Duration = Duration::from_millis(8);
/// The queue is the least of this many last samples beyond the path's
/// least: Wi-Fi's own jitter (a retry, a beacon) is no queue; one that
/// stays is (LEDBAT's current delay filter).
const RECENT: usize = 2;
/// The least round trip is the least of this long (and the one before).
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Bulk never slower than this (control messages go on). (256 KiB/s was
/// most of a 3 Mbit/s home Wi-Fi: the realtime waited in the router's
/// queue behind the bulk, 260 ms, on an emulated one.)
const MIN_RATE: f64 = 32.0 * 1024.0;
/// Paced faster than this: not paced.
const UNPACED: f64 = 200e6;
/// At most one slowing down this often (a sample's queue drains in it).
const SLOW_DOWN_EVERY: Duration = Duration::from_millis(50);

pub struct Pace {
    recent: [u64; RECENT],
    next: usize,
    least: u64,          // µs, this window
    least_before: u64,   // µs, the window before
    window: Instant,
    /// Bytes a second the reliable frames may go at; None: not paced.
    rate: Option<f64>,
    tokens: f64,
    refilled: Instant,
    written: u64,        // reliable bytes since the last sample
    sampled: Instant,
    throughput: f64,     // what went, smoothed (bytes a second)
    slowed: Instant,
}

impl Pace {
    pub fn new(now: Instant) -> Pace {
        Pace { recent: [u64::MAX; RECENT], next: 0, least: u64::MAX, least_before: u64::MAX, window: now, rate: None,
               tokens: 0.0, refilled: now, written: 0,
               sampled: now, throughput: 0.0, slowed: now - SLOW_DOWN_EVERY }
    }

    /// A round trip (µs) on the realtime queue; busy: realtime goes now.
    pub fn sample(&mut self, rtt: u64, busy: bool, now: Instant) {
        if now.duration_since(self.window) > BASE_WINDOW {
            self.least_before = self.least;
            self.least = u64::MAX;
            self.window = now;
        }
        self.least = self.least.min(rtt);
        self.recent[self.next] = rtt;
        self.next = (self.next + 1) % RECENT;
        let elapsed = now.duration_since(self.sampled).as_secs_f64();
        if elapsed > 0.0 {
            self.throughput = 0.7 * self.throughput + 0.3 * (self.written as f64 / elapsed);
        }
        self.written = 0;
        self.sampled = now;
        if !busy {
            self.rate = None;
            return;
        }
        let now_least = self.recent.iter().copied().min().unwrap_or(rtt);
        let queued = now_least.saturating_sub(self.least.min(self.least_before));
        let target = TARGET.as_micros() as u64;
        if queued > target {
            if now.duration_since(self.slowed) >= SLOW_DOWN_EVERY {
                let from = self.rate.unwrap_or(f64::MAX).min(self.throughput.max(MIN_RATE));
                self.rate = Some((from * 0.8).max(MIN_RATE));
                self.slowed = now;
                self.refilled = now;
                self.tokens = self.tokens.min(0.0);
            }
        } else if queued < target / 2 {
            // Room: faster again (between half the target and the target, as it is).
            if let Some(rate) = self.rate {
                let faster = rate * 1.15;
                self.rate = if faster > UNPACED { None } else { Some(faster) };
            }
        }
    }

    fn refill(&mut self, now: Instant) {
        if let Some(rate) = self.rate {
            let elapsed = now.duration_since(self.refilled).as_secs_f64();
            // A slice's worth at most saved up: no burst after a pause.
            self.tokens = (self.tokens + rate * elapsed).min(16.0 * 1024.0);
        }
        self.refilled = now;
    }

    /// Whether reliable frames may go now (then a slice of them; what goes
    /// beyond the tokens is paid back before the next).
    pub fn may_send(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.rate.is_none() || self.tokens > 0.0
    }

    /// Reliable bytes handed to the stream.
    pub fn spent(&mut self, bytes: usize) {
        self.written += bytes as u64;
        if self.rate.is_some() {
            self.tokens -= bytes as f64;
        }
    }

    /// While paced and in debt: how long until reliable frames may go.
    pub fn wait(&self) -> Option<Duration> {
        match self.rate {
            Some(rate) if self.tokens <= 0.0 => Some(Duration::from_secs_f64((-self.tokens + 1.0) / rate)),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn paced(&self) -> Option<f64> {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn not_paced_without_realtime() {
        let t = Instant::now();
        let mut pace = Pace::new(t);
        pace.spent(10 << 20);
        pace.sample(80_000, false, t + ms(100));  // a long queue, but nothing realtime
        assert!(pace.paced().is_none() && pace.may_send(t + ms(100)));
    }

    #[test]
    fn slows_down_as_the_queue_grows_and_speeds_up_as_it_drains() {
        let t = Instant::now();
        let mut pace = Pace::new(t);
        pace.sample(5_000, true, t);  // the path's least: 5 ms
        pace.spent(2 << 20);
        pace.sample(30_001, false, t + ms(100));  // a queue growing (20 MB/s went), nothing realtime yet
        pace.sample(30_000, true, t + ms(110));  // 25 ms queued, and stays
        let slower = pace.paced().expect("paced");
        assert!(slower > MIN_RATE && slower < 20e6, "{slower}");  // under what went
        pace.sample(30_000, true, t + ms(120));  // again within SLOW_DOWN_EVERY: once
        assert_eq!(pace.paced(), Some(slower));
        pace.sample(31_000, true, t + ms(200));
        let slowest = pace.paced().unwrap();
        assert!(slowest < slower);
        pace.sample(6_000, true, t + ms(250));  // drained: one sample is enough to see it
        assert!(pace.paced().unwrap() > slowest);
        pace.sample(6_000, false, t + ms(300));  // realtime over
        assert!(pace.paced().is_none());
    }

    #[test]
    fn paced_bytes_wait_their_turn() {
        let t = Instant::now();
        let mut pace = Pace::new(t);
        pace.sample(5_000, true, t);
        pace.spent(1 << 20);
        pace.sample(40_000, false, t + ms(10));
        pace.sample(40_000, true, t + ms(100));
        let rate = pace.paced().unwrap();
        assert!(!pace.may_send(t + ms(100)));  // no tokens yet
        let later = t + ms(100) + Duration::from_secs_f64(20_000.0 / rate);
        assert!(pace.may_send(later));
        pace.spent(64 << 10);  // a frame beyond the tokens: a debt
        let wait = pace.wait().unwrap();
        assert!(wait > ms(0) && !pace.may_send(later));
        assert!(pace.may_send(later + wait + ms(1)));
    }

    #[test]
    fn a_spike_is_no_queue() {
        let t = Instant::now();
        let mut pace = Pace::new(t);
        for i in 0..2 {
            pace.sample(5_000, true, t + ms(10 * i));
        }
        pace.spent(1 << 20);
        pace.sample(60_000, true, t + ms(100));  // one retry's worth
        assert!(pace.paced().is_none());
    }

    #[test]
    fn never_slower_than_the_floor() {
        let t = Instant::now();
        let mut pace = Pace::new(t);
        pace.sample(5_000, true, t);
        for i in 1..100 {
            pace.sample(90_000, true, t + ms(60 * i));
        }
        assert!(pace.paced().unwrap() >= MIN_RATE);
    }
}
