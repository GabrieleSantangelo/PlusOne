//! Who may start a handshake. Anyone on the network can try, and each try
//! costs the tablet a signature by its keystore before it knows who asks
//! (TLS 1.3: the server signs first), so: so many at once, so many from
//! one address, and new ones at a rate, each address's and all together.
//! A stranger's flood holds up no one else, and costs little. The TCP and
//! the QUIC listeners share one.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Handshakes at once, from anywhere; from one address.
pub(crate) const AT_ONCE: usize = 8;
const AT_ONCE_FROM_ONE: usize = 2;
/// New handshakes: so many at once (a burst), then so many a second; from
/// one address, then from all. A device that comes back dials TCP and QUIC
/// together, and again a few seconds later if a path is lost.
const BURST_FROM_ONE: f64 = 4.0;
const RATE_FROM_ONE: f64 = 1.0;
const BURST: f64 = 16.0;
const RATE: f64 = 8.0;
/// Addresses remembered at most; past this, those quiet for long are forgotten.
const SOURCES: usize = 256;
const QUIET: Duration = Duration::from_secs(10);

pub(crate) struct Gate {
    state: Mutex<State>,
}

struct State {
    running: usize,
    bucket: Bucket,
    sources: HashMap<IpAddr, Source>,
}

struct Source {
    running: usize,
    bucket: Bucket,
}

struct Bucket {
    tokens: f64,
    refilled: Instant,
}

impl Bucket {
    fn full(burst: f64, now: Instant) -> Bucket {
        Bucket { tokens: burst, refilled: now }
    }

    fn refill(&mut self, burst: f64, rate: f64, now: Instant) {
        self.tokens = (self.tokens + rate * now.duration_since(self.refilled).as_secs_f64()).min(burst);
        self.refilled = now;
    }
}

/// A handshake let in: counted until it is dropped.
pub(crate) struct Handshake {
    gate: Arc<Gate>,
    from: IpAddr,
}

impl Gate {
    pub(crate) fn new() -> Arc<Gate> {
        Arc::new(Gate { state: Mutex::new(State { running: 0, bucket: Bucket::full(BURST, Instant::now()),
                                                  sources: HashMap::new() }) })
    }

    /// Handshakes under way now.
    pub(crate) fn running(&self) -> usize {
        self.state.lock().unwrap().running
    }

    /// One more from there, if it may start now.
    pub(crate) fn start(self: &Arc<Self>, from: IpAddr) -> Option<Handshake> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        if state.sources.len() >= SOURCES {
            state.sources.retain(|_, source| source.running > 0 || now.duration_since(source.bucket.refilled) < QUIET);
            if state.sources.len() >= SOURCES {
                return None;
            }
        }
        if state.running >= AT_ONCE {
            return None;
        }
        state.bucket.refill(BURST, RATE, now);
        if state.bucket.tokens < 1.0 {
            return None;
        }
        let source = state.sources.entry(from)
            .or_insert_with(|| Source { running: 0, bucket: Bucket::full(BURST_FROM_ONE, now) });
        source.bucket.refill(BURST_FROM_ONE, RATE_FROM_ONE, now);
        if source.running >= AT_ONCE_FROM_ONE || source.bucket.tokens < 1.0 {
            return None;
        }
        source.running += 1;
        source.bucket.tokens -= 1.0;
        state.bucket.tokens -= 1.0;
        state.running += 1;
        Some(Handshake { gate: self.clone(), from })
    }
}

impl Drop for Handshake {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap();
        state.running -= 1;
        if let Some(source) = state.sources.get_mut(&self.from) {
            source.running -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_address_cannot_hold_up_the_others() {
        let gate = Gate::new();
        let (stranger, device): (IpAddr, IpAddr) = ([10, 0, 0, 66].into(), [10, 0, 0, 2].into());
        let held: Vec<_> = (0..10).filter_map(|_| gate.start(stranger)).collect();
        assert_eq!(held.len(), AT_ONCE_FROM_ONE);
        assert!(gate.start(device).is_some());
        drop(held);
        // At once no more, but at a rate: its burst is spent.
        let again: Vec<_> = (0..10).filter_map(|_| gate.start(stranger).map(drop)).collect();
        assert_eq!(again.len(), BURST_FROM_ONE as usize - AT_ONCE_FROM_ONE);
        assert!(gate.start(device).is_some());
        assert_eq!(gate.running(), 0);
    }

    #[test]
    fn many_addresses_at_once_are_held_to_the_whole() {
        let gate = Gate::new();
        let held: Vec<_> = (0..64u8).filter_map(|i| gate.start([10, 0, 1, i].into())).collect();
        assert_eq!(held.len(), AT_ONCE);
        drop(held);
        let started = (0..64u8).filter_map(|i| gate.start([10, 0, 2, i].into()).map(drop)).count();
        assert_eq!(started, BURST as usize - AT_ONCE);
    }
}
