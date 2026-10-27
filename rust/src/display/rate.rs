//! The bitrate the network takes, from the app's report each second
//! (appdisplay.py RateControl): down by a third at once when frames were
//! lost, the delay through the network grew (a queue filling somewhere), or
//! SKIPPED frames or more were not encoded for what waited to go out here;
//! up by a tenth after a few clean seconds, never above the one set (most)
//! nor below MIN_KBPS. A frame or two not encoded (after a keyframe) is not
//! clean, nor a reason to count the clean ones again.
//!
//! Where a queue showed (ceiling: not a frame lost now and then, which any
//! Wi-Fi does) is remembered for CEILING_FOR reports: up to NEAR of it the
//! bitrate goes back quickly, past that only by PROBE every few clean
//! seconds. Past it with no trouble, or once it is old, it is forgotten; a
//! drop well under it (the same queue a second later) leaves it.

use std::collections::VecDeque;

pub const MIN_KBPS: u32 = 1500;
const GROWN_MS: f64 = 25.0;  // over the least: a queue
const CLEAN: u32 = 3;        // reports without loss nor queue before going up
const HISTORY: usize = 10;   // reports the least is taken over
const FRAMES: u32 = 10;      // shown in the second, for its delay to count
const SKIPPED: u32 = 3;      // frames not encoded in the second: the network behind
const NEAR: f64 = 0.85;      // of the ceiling: up to here quickly
const PROBE: f64 = 1.03;     // past it, up by this...
const PROBE_EVERY: u32 = 10; // ...after this many clean reports
const CEILING_FOR: u32 = 60; // reports (seconds) a ceiling holds

pub struct RateControl {
    pub most: u32,
    pub kbps: u32,
    clean: u32,
    pub ceiling: Option<u32>,
    ceiling_age: u32,
    delays: VecDeque<f64>,
}

impl RateControl {
    pub fn new(most: u32) -> RateControl {
        RateControl { most, kbps: most, clean: 0, ceiling: None, ceiling_age: 0, delays: VecDeque::new() }
    }

    /// A report, and the frames not encoded since the last; the new bitrate if it changed.
    pub fn report(&mut self, shown: u32, lost: u32, capture_to_decoded: f64, first_to_decoded: f64, skipped: u32) -> Option<u32> {
        let delay = (shown >= FRAMES).then_some(capture_to_decoded - first_to_decoded);
        if let Some(delay) = delay {
            if self.delays.len() == HISTORY {
                self.delays.pop_front();
            }
            self.delays.push_back(delay);
        }
        let least = self.delays.iter().copied().fold(f64::INFINITY, f64::min);
        let grown = delay.is_some_and(|d| self.delays.len() >= 3 && d > least + GROWN_MS);
        self.ceiling_age += 1;
        if self.ceiling.is_some() && self.ceiling_age > CEILING_FOR {
            self.ceiling = None;
        }
        let queue = grown || skipped >= SKIPPED;
        if lost > 0 || queue {
            self.clean = 0;
            if queue && self.ceiling.is_none_or(|c| self.kbps as f64 >= c as f64 * NEAR) {
                self.ceiling = Some(self.kbps);
                self.ceiling_age = 0;
            }
            return self.set(self.kbps * 2 / 3);
        }
        if skipped > 0 {
            return None;
        }
        self.clean += 1;
        if self.kbps >= self.most {
            return None;
        }
        if let Some(ceiling) = self.ceiling.filter(|&c| self.kbps as f64 >= c as f64 * NEAR) {
            if self.clean < PROBE_EVERY {
                return None;
            }
            self.clean = 0;
            let kbps = self.set((self.kbps as f64 * PROBE) as u32);
            if self.kbps >= ceiling {
                self.ceiling = None;  // it takes as much now
            }
            return kbps;
        }
        if self.clean >= CLEAN {
            self.clean = 0;
            let mut up = self.kbps * 11 / 10 + 100;
            if let Some(ceiling) = self.ceiling {
                up = up.min((ceiling as f64 * NEAR) as u32);
            }
            return self.set(up.max(self.kbps + 1));
        }
        None
    }

    fn set(&mut self, kbps: u32) -> Option<u32> {
        let kbps = kbps.clamp(MIN_KBPS, self.most);
        if kbps == self.kbps {
            return None;
        }
        self.kbps = kbps;
        Some(kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bitrate_follows_the_network() {
        let mut rate = RateControl::new(20000);
        for _ in 0..5 {
            assert_eq!(rate.report(60, 0, 30.0, 20.0, 0), None);
        }
        assert_eq!(rate.report(55, 3, 30.0, 20.0, 0), Some(13333));  // frames lost: down by a third
        assert_eq!(rate.report(60, 0, 80.0, 20.0, 0), Some(8888));   // the delay grew: a queue
        assert_eq!(rate.report(0, 40, 0.0, 0.0, 0), Some(5925));
        for _ in 0..2 {
            assert_eq!(rate.report(60, 0, 30.0, 20.0, 0), None);
        }
        assert_eq!(rate.report(60, 0, 30.0, 20.0, 0), Some(6617));   // clean a while: up by a tenth
        rate.kbps = MIN_KBPS;
        assert_eq!(rate.report(60, 9, 30.0, 20.0, 0), None);         // never below the least
    }

    #[test]
    fn frames_not_encoded_for_the_queue_here_lower_the_bitrate() {
        let mut rate = RateControl::new(20000);
        rate.kbps = 10000;
        for _ in 0..2 {
            assert_eq!(rate.report(60, 0, 30.0, 20.0, 0), None);
        }
        assert_eq!(rate.report(60, 0, 30.0, 20.0, 2), None);  // after a keyframe: not clean...
        assert_eq!(rate.report(60, 0, 30.0, 20.0, 0), Some(11100));  // ...nor counting again
        assert_eq!(rate.report(45, 0, 30.0, 20.0, 14), Some(7400));
    }

    #[test]
    fn the_bitrate_goes_back_slowly_where_the_network_gave_up() {
        let mut rate = RateControl::new(20000);
        assert_eq!(rate.report(40, 0, 30.0, 20.0, 15), Some(13333));
        let ups: Vec<u32> = (0..9).filter_map(|_| rate.report(60, 0, 30.0, 20.0, 0)).collect();
        assert_eq!(ups, vec![14766, 16342, 17000]);  // quickly, up to 85 % of it
        let ups: Vec<u32> = (0..30).filter_map(|_| rate.report(60, 0, 30.0, 20.0, 0)).collect();
        assert_eq!(ups, vec![17510, 18035, 18576]);  // past that, 3 % every 10 s
    }

    #[test]
    fn a_frame_lost_now_and_then_is_no_ceiling() {
        let mut rate = RateControl::new(20000);
        let mut kbps = Vec::new();
        for second in 0..300 {
            rate.report(60, (second % 30 == 29) as u32, 30.0, 20.0, 0);
            kbps.push(rate.kbps);
        }
        assert!(rate.ceiling.is_none());
        assert!(kbps[180..].iter().map(|&k| k as f64).sum::<f64>() / 120.0 > 15000.0);
    }

    #[test]
    fn one_queue_over_two_reports_is_one_ceiling() {
        let mut rate = RateControl::new(20000);
        rate.report(40, 0, 30.0, 20.0, 15);
        rate.report(40, 0, 30.0, 20.0, 6);
        assert_eq!((rate.kbps, rate.ceiling), (8888, Some(20000)));
    }

    #[test]
    fn a_keyframe_s_skipped_frame_does_not_keep_the_bitrate_down() {
        let mut rate = RateControl::new(20000);
        rate.report(40, 0, 30.0, 20.0, 15);
        for second in 0..120 {
            rate.report(60, 0, 30.0, 20.0, (second % 10 == 9) as u32);
        }
        assert_eq!((rate.kbps, rate.ceiling), (20000, None));
    }

    #[test]
    fn a_still_desktop_s_few_frames_do_not_move_the_bitrate() {
        let mut rate = RateControl::new(20000);
        for _ in 0..4 {
            rate.report(30, 0, 30.0, 20.0, 0);
        }
        assert_eq!(rate.report(1, 0, 90.0, 20.0, 0), None);  // one frame, late: Wi-Fi's jitter
        assert_eq!(rate.report(30, 0, 90.0, 20.0, 0), Some(13333));
    }
}
