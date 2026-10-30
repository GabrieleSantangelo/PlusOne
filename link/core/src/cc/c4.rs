//! C4, "Christian's Congestion Control Code" (Christian Huitema, Private Octopus): a congestion
//! controller for real-time media, ported to quinn. It keeps two variables: the nominal rate (the
//! largest delivery rate measured outside congestion; measurements only raise it) and the nominal
//! max RTT (the largest round trip seen while sending at most at that rate, so it holds the path's
//! jitter, capped at the running min RTT plus 250 ms and decaying by 1/8 an era). It paces at alpha
//! times the nominal rate and lets that rate times the max RTT, plus a margin of min(15 ms, max
//! RTT / 4), be in flight. Alpha follows the state: Initial 2, Recovery 15/16 (7/8 or 3/4 while a
//! backlog drains), Cruising 1, Probing 17/16 (33/32 after ECN), Pushing 1256/1024. States change
//! at the end of an era (a round trip: the first packet sent in it is acknowledged) or on a
//! congestion signal: an RTT over the max RTT plus a delay threshold (acted on only while probing
//! or pushing), a smoothed loss rate or an ECN-CE rate over a threshold. The thresholds tighten as
//! the rate grows ("sensitivity"), so faster flows yield first. A signal enters Recovery and cuts
//! the nominal rate by beta (at most 1/4) unless C4 was probing or pushing; two congested recoveries
//! in a row reset the nominal rate to the largest rate measured since the previous one; a cycle
//! without growth nor congestion trims it to the larger of the last two cycles' largest rates.
//!
//! Sources: draft-huitema-ccwg-c4-spec-04 (2026-07-19) and draft-huitema-ccwg-c4-design-04
//! (2026-07-05), with their newer editor's copies (trimming, draining, Wi-Fi) in
//! github.com/private-octopus/c4 at 9982352e376c52b7767d431dd651413a9789e334; the code,
//! picoquic/c4.c in github.com/private-octopus/picoquic at
//! 3012b39ca4190cb09984437ed403417be97f2f5a, with the picoquic parts it reads (frames.c's delivery
//! rate, timing.c's RTT sample, loss_recovery.c's loss reports, cc_common.c's era test). Where the
//! drafts and the code disagree this follows the code. Each function names the C function it ports.
//!
//! quinn has one control, window(): it lets that much be in flight and paces at 1.25 window /
//! smoothed RTT. The window here is C4's congestion window as c4.c computes it, so the jitter
//! allowance C4 is built around stays. C4's pacing rate has no hook in quinn (quinn's pacing is
//! looser by the max RTT over the smoothed RTT); budget() hands it to the application, which in
//! this program sets the sending rate. The wcap parameter caps the window at wcap times the pacing
//! rate times the smoothed RTT, for traffic that does not follow the budget.
//!
//! quinn reports acknowledgements packet by packet, then the end of the ACK frame, then its losses
//! and ECN; picoquic gives C4 the RTT sample, then the losses, then the acknowledgement. The ACK
//! frame's RTT sample is handled at on_end_acks and its acknowledgement at the next event that is
//! not a loss or ECN report of the same instant, which restores picoquic's order.

// Ported from picoquic/c4.c, whose notice follows verbatim:
//
// Author: Christian Huitema
// Copyright (c) 2025, Private Octopus, Inc.
// All rights reserved.
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
// ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
// WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL Private Octopus, Inc. BE LIABLE FOR ANY
// DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
// (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
// LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
// ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//
// The C4 repository (drafts, design papers, test specifications) carries this notice verbatim:
//
// MIT License
//
// Copyright (c) 2025 Private Octopus
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use super::shared::Report;
use quinn::congestion::{Controller, ControllerMetrics};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

// c4.c's constants: times in microseconds, rates in bytes a second, gains in 1/1024.
const ALPHA_NEUTRAL: u64 = 1024;
const ALPHA_RECOVER2: u64 = 896;
const ALPHA_CRUISE: u64 = 1024;
const ALPHA_PUSH_LOW: u64 = 1088;
const ALPHA_PUSH_VERY_LOW: u64 = 1056;
const ALPHA_PUSH_50: u64 = 1536;
const ALPHA_PUSH_200: u64 = 3072;
const ALPHA_INITIAL: u64 = 2048;
const ALPHA_DRAINING: u64 = 758;
const ALPHA_DRAINING_MID: u64 = 896;
const NB_PACKETS_BEFORE_LOSS: u64 = 20;
const NB_CRUISE_BEFORE_PUSH: u64 = 2;
const MAX_RTT_MIN: u64 = 1000;
const ECN_SHIFT_G: u32 = 4;
const PROBE_LEVEL_MAX: u32 = 3;
const PROBE_LEVEL_DEFAULT: u32 = 1;
const INITIAL_PACING: u64 = 0x20000;
// picoquic's, which c4.c relies on.
const INITIAL_RTT: u64 = 250_000;
const SMOOTHED_LOSS_SCOPE: u64 = 32;
const SMOOTHED_LOSS_FACTOR: f64 = 1.0 / 16.0;
const BANDWIDTH_TIME_INTERVAL_MIN: u64 = 1000;
// The port's own.
/// Send batches remembered, for the delivery rate and the packet numbers of losses.
const SENT_RECORDS: usize = 2048;
/// Under u32::MAX, past which quinn stops pacing.
const MAX_WINDOW: u64 = 1 << 31;
/// Lost packets replayed one by one from a quinn loss event (c4.c hears of each).
const MAX_LOST_PACKETS: u64 = 64;

/// The tunables worth changing live. Times in milliseconds, rates in bytes a second, gains as
/// plain factors; the defaults are c4.c's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct C4Params {
    /// Cap of the delay threshold (C4_DELAY_THRESHOLD_MAX, 25 ms; else 1/16 to 1/4 of the max RTT).
    pub(crate) delay_max: f64,
    /// Cap of the window's RTT margin (C4_RTT_MARGIN_DELAY, 15 ms; else max RTT / 4).
    pub(crate) margin: f64,
    /// Most the max RTT may stand over the running min RTT (C4_MAX_JITTER, 250 ms).
    pub(crate) jitter_max: f64,
    /// Loss rate threshold at full sensitivity (0.02 in c4_loss_threshold(); 0.52 at none).
    pub(crate) loss_base: f64,
    /// Largest cut of the nominal rate on a signal (C4_BETA_LOSS_1024, 1/4).
    pub(crate) beta: f64,
    /// Pushing gain (C4_ALPHA_PUSH_25, 1256/1024).
    pub(crate) push: f64,
    /// Probing gain (probe level 1, C4_ALPHA_PUSH_LOW_1024, 17/16).
    pub(crate) probe: f64,
    /// Recovery gain with no backlog (C4_ALPHA_RECOVER_1024, 15/16).
    pub(crate) recover: f64,
    /// Floor of the nominal rate after a cut (the port's; 0 is none, as in c4.c).
    pub(crate) min_rate: f64,
    /// Floor of the window in packets (the port's; the spec says 2).
    pub(crate) min_win: f64,
    /// Cap of the window in pacing rate times smoothed RTT (the port's; 0 is off).
    pub(crate) wcap: f64,
}

impl Default for C4Params {
    fn default() -> Self {
        C4Params {
            delay_max: 25.0,
            margin: 15.0,
            jitter_max: 250.0,
            loss_base: 0.02,
            beta: 0.25,
            push: 1256.0 / 1024.0,
            probe: 1088.0 / 1024.0,
            recover: 960.0 / 1024.0,
            min_rate: 0.0,
            min_win: 4.0,
            wcap: 0.0,
        }
    }
}

impl C4Params {
    /// Parses one key=value pair into its field.
    pub(crate) fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v: f64 = value.trim().parse().map_err(|_| format!("{key}: {value:?} is not a number"))?;
        let (slot, lo, hi) = match key {
            "delay_max" => (&mut self.delay_max, 1.0, 200.0),
            "margin" => (&mut self.margin, 0.0, 100.0),
            "jitter_max" => (&mut self.jitter_max, 10.0, 2000.0),
            "loss_base" => (&mut self.loss_base, 0.0, 0.5),
            "beta" => (&mut self.beta, 0.05, 0.5),
            "push" => (&mut self.push, 1.03, 2.0),
            "probe" => (&mut self.probe, 1.0, 1.5),
            "recover" => (&mut self.recover, 0.5, 1.0),
            "min_rate" => (&mut self.min_rate, 0.0, 12_500_000.0),
            "min_win" => (&mut self.min_win, 2.0, 64.0),
            "wcap" => (&mut self.wcap, 0.0, 8.0),
            _ => return Err(format!("unknown key {key:?}")),
        };
        if !(lo..=hi).contains(&v) {
            return Err(format!("{key} must be within {lo}..={hi}"));
        }
        if key == "min_win" && v.fract() != 0.0 {
            return Err("min_win must be a whole number of packets".to_string());
        }
        if key == "wcap" && v > 0.0 && v < 0.5 {
            return Err("wcap must be 0 (off) or within 0.5..=8".to_string());
        }
        *slot = v;
        Ok(())
    }

    /// All of them as "key=value ...", in a fixed order.
    pub(crate) fn describe(&self) -> String {
        format!(
            "delay_max={} margin={} jitter_max={} loss_base={} beta={} push={} probe={} recover={} min_rate={} \
             min_win={} wcap={}",
            self.delay_max, self.margin, self.jitter_max, self.loss_base, self.beta, self.push, self.probe,
            self.recover, self.min_rate, self.min_win, self.wcap
        )
    }
}

/// c4.c: c4_alg_state_t (no Resuming: quinn has no careful-resume seed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AlgState {
    Initial,
    Recovery,
    Cruising,
    Probing,
    Pushing,
}

/// c4.c: c4_congestion_t
#[derive(Clone, Copy, PartialEq, Eq)]
enum Signal {
    None,
    Delay,
    Ecn,
    Loss,
}

/// c4.c: c4_growing_t
#[derive(Clone, Copy, PartialEq, Eq)]
enum Growth {
    None,
    Slow,
    Fast,
}

/// One quinn transmit (on_sent): its packet numbers, time, bytes, and the delivery state
/// picoquic stamps on each packet (delivered_prior, delivered_time_prior, delivered_sent_prior).
#[derive(Clone, Copy)]
struct Sent {
    first: u64,
    last: u64,
    time: u64,
    bytes: u64,
    delivered: u64,
    delivered_time: u64,
    delivered_sent: u64,
}

/// An ACK frame waiting for its acknowledgement notification (picoquic_per_ack_state_t).
#[derive(Clone, Copy)]
struct Ack {
    now: u64,
    bytes: u64,
    rtt: u64,
    delivered_since: u64,
}

/// The controller. Times are microseconds from its creation (from 1: c4.c uses 0 for "never"),
/// rates bytes a second, gains 1/1024, as in c4.c.
#[derive(Clone)]
pub(crate) struct C4 {
    params: C4Params,
    epoch: Instant,
    mtu: u64,
    // c4.c: c4_state_t
    alg_state: AlgState,
    nominal_rate: u64,
    nominal_max_rtt: u64,
    initial_cwnd: u64,
    backlog_1024: u64,
    initial_last_time: u64,
    running_min_rtt: u64,
    alpha_1024_current: u64,
    alpha_1024_previous: u64,
    nb_packets_in_startup: u64,
    era_sequence: u64,
    nb_cruise_left_before_push: u64,
    recent_maximum_rate: u64,
    previous_maximum_rate: u64,
    recent_congestions: u32,
    probe_level: u32,
    nb_eras_no_increase: u32,
    push_rate_old: u64,
    push_alpha: u64,
    era_max_rtt: u64,
    era_min_rtt: u64,
    delay_threshold: u64,
    recent_delay_excess: u64,
    last_lost_packet_number: u64,
    smoothed_drop_rate: f64,
    ecn_alpha: u64,
    ecn_ect1: u64,
    ecn_ce: u64,
    ecn_threshold: u64,
    congestion_notified: bool,
    push_was_not_limited: bool,
    excess_ce_after_push: bool,
    // picoquic_path_t's fields that c4.c reads or sets.
    cwin: u64,
    pacing_rate: u64,
    smoothed_rtt: u64,
    rtt_sample: u64,
    delivered: u64,
    delivered_last: u64,
    delivered_time_last: u64,
    delivered_sent_last: u64,
    bandwidth_estimate: u64,
    max_sample_delivered: u64,
    max_sample_acked_time: u64,
    max_sample_sent_time: u64,
    peak_bandwidth_estimate: u64,
    send_sequence: u64,
    highest_acknowledged: Option<u64>,
    last_time_acked_data_frame_sent: u64,
    last_sender_limited_time: u64,
    /// Packets acknowledged and ECN-CE reports: quinn's stand-ins for the ECT(1) and CE counts.
    ecn_acked_total: u64,
    ecn_ce_total: u64,
    // quinn glue.
    sent: VecDeque<Sent>,
    batch_bytes: u64,
    batch_count: u64,
    batch_newest: u64,
    pending: Option<Ack>,
}

fn mult1024(c: u64, v: u64) -> u64 {
    ((v as u128 * c as u128) >> 10).min(u64::MAX as u128) as u64
}

/// picoquic_utils.h: PICOQUIC_BYTES_FROM_RATE
fn bytes_from_rate(us: u64, rate: u64) -> u64 {
    (us as u128 * rate as u128 / 1_000_000).min(u64::MAX as u128) as u64
}

/// picoquic_utils.h: PICOQUIC_RATE_FROM_BYTES
fn rate_from_bytes(bytes: u64, us: u64) -> u64 {
    if us == 0 {
        return 0;
    }
    (bytes as u128 * 1_000_000 / us as u128).min(u64::MAX as u128) as u64
}

fn q10(gain: f64) -> u64 {
    (gain * 1024.0).round() as u64
}

fn us_of_ms(ms: f64) -> u64 {
    (ms * 1000.0) as u64
}

impl C4 {
    // c4.c: c4_init(), c4_reset()
    pub(crate) fn new(now: Instant, mtu: u16, params: C4Params) -> C4 {
        let mtu = u64::from(mtu.max(1));
        let mut c4 = C4 {
            params,
            epoch: now,
            mtu,
            alg_state: AlgState::Initial,
            nominal_rate: 0,
            nominal_max_rtt: 0,
            initial_cwnd: 0,
            backlog_1024: 0,
            initial_last_time: 0,
            running_min_rtt: u64::MAX,
            alpha_1024_current: ALPHA_INITIAL,
            alpha_1024_previous: 0,
            nb_packets_in_startup: 0,
            era_sequence: 0,
            nb_cruise_left_before_push: 0,
            recent_maximum_rate: 0,
            previous_maximum_rate: 0,
            recent_congestions: 0,
            probe_level: 0,
            nb_eras_no_increase: 0,
            push_rate_old: 0,
            push_alpha: 0,
            era_max_rtt: 0,
            era_min_rtt: u64::MAX,
            delay_threshold: 0,
            recent_delay_excess: 0,
            last_lost_packet_number: 0,
            smoothed_drop_rate: 0.0,
            ecn_alpha: 0,
            ecn_ect1: 0,
            ecn_ce: 0,
            ecn_threshold: 0,
            congestion_notified: false,
            push_was_not_limited: false,
            excess_ce_after_push: false,
            cwin: 10 * mtu,
            pacing_rate: 0,
            // A path starts with picoquic's initial RTT, which the first Initial takes as its max RTT.
            smoothed_rtt: INITIAL_RTT,
            rtt_sample: 0,
            delivered: 0,
            delivered_last: 0,
            delivered_time_last: 0,
            delivered_sent_last: 0,
            bandwidth_estimate: 0,
            max_sample_delivered: 0,
            max_sample_acked_time: 0,
            max_sample_sent_time: 0,
            peak_bandwidth_estimate: 0,
            send_sequence: 0,
            highest_acknowledged: None,
            last_time_acked_data_frame_sent: 0,
            last_sender_limited_time: 0,
            ecn_acked_total: 0,
            ecn_ce_total: 0,
            sent: VecDeque::new(),
            batch_bytes: 0,
            batch_count: 0,
            batch_newest: 0,
            pending: None,
        };
        c4.enter_initial();
        c4
    }

    fn time(&self, t: Instant) -> u64 {
        1 + t.saturating_duration_since(self.epoch).as_micros().min(1 << 62) as u64
    }

    /// picoquic_internal.h: PICOQUIC_CWIN_INITIAL, in quinn's MTU.
    fn cwin_initial(&self) -> u64 {
        10 * self.mtu
    }

    // c4.c: c4_sensitivity_1024()
    fn sensitivity_1024(&self) -> u64 {
        let rate = self.nominal_rate;
        if rate < 50_000 {
            0
        } else if rate > 10_000_000 {
            1024
        } else if rate < 1_000_000 {
            (rate - 50_000) * 963 / 950_000
        } else {
            963 + (rate - 1_000_000) * 61 / 9_000_000
        }
    }

    // c4.c: c4_delay_threshold()
    fn compute_delay_threshold(&self) -> u64 {
        let fraction = 64 + mult1024(1024 - self.sensitivity_1024(), 196);
        mult1024(fraction, self.nominal_max_rtt).min(us_of_ms(self.params.delay_max))
    }

    // c4.c: c4_ecn_threshold()
    fn compute_ecn_threshold(&self) -> u64 {
        192 - mult1024(self.sensitivity_1024(), 96)
    }

    // c4.c: c4_loss_threshold()
    fn loss_threshold(&self) -> f64 {
        self.params.loss_base + 0.50 * (1.0 - self.sensitivity_1024() as f64 / 1024.0)
    }

    // c4.c: c4_update_loss_rate()
    fn update_loss_rate(&mut self, lost_packet_number: u64) {
        let mut next = self.last_lost_packet_number;
        if lost_packet_number > next {
            if next.saturating_add(SMOOTHED_LOSS_SCOPE) < lost_packet_number {
                next = lost_packet_number - SMOOTHED_LOSS_SCOPE;
            }
            while next < lost_packet_number {
                self.smoothed_drop_rate *= 1.0 - SMOOTHED_LOSS_FACTOR;
                next += 1;
            }
            self.smoothed_drop_rate += (1.0 - self.smoothed_drop_rate) * SMOOTHED_LOSS_FACTOR;
            self.last_lost_packet_number = lost_packet_number;
        }
    }

    // c4.c: c4_update_ecn_alpha(). quinn gives neither ECT(1) nor CE counts: the packets acknowledged
    // stand for the marked ones and each ECN-CE report for one CE mark.
    fn update_ecn_alpha(&mut self) {
        let delta_ce = self.ecn_ce_total.saturating_sub(self.ecn_ce);
        let delta_all = self.ecn_acked_total.saturating_sub(self.ecn_ect1).max(delta_ce);
        self.ecn_ect1 = self.ecn_acked_total;
        self.ecn_ce = self.ecn_ce_total;
        if delta_all > 0 {
            let frac = delta_ce.saturating_mul(1024) / delta_all;
            if frac > self.ecn_alpha && frac >= 512 {
                self.ecn_alpha = frac;
            } else {
                self.ecn_alpha = ((self.ecn_alpha << ECN_SHIFT_G) - self.ecn_alpha + frac) >> ECN_SHIFT_G;
            }
        }
    }

    // c4.c: c4_apply_rate_and_cwin(). picoquic's pacing quantum has no quinn equivalent.
    fn apply_rate_and_cwin(&mut self) {
        let mut pacing_rate = mult1024(self.alpha_1024_current, self.nominal_rate);
        let mut target_cwin = self.cwin_initial();
        if self.nominal_max_rtt != 0 && self.nominal_rate != 0 {
            target_cwin = bytes_from_rate(self.nominal_max_rtt, pacing_rate);
        }
        if self.alg_state == AlgState::Initial {
            target_cwin = target_cwin.max(self.initial_cwnd);
            if self.nb_packets_in_startup > 0 {
                pacing_rate = pacing_rate.max(INITIAL_PACING);
                if self.peak_bandwidth_estimate > pacing_rate {
                    pacing_rate = pacing_rate.saturating_add(self.peak_bandwidth_estimate) / 2;
                    let min_win = bytes_from_rate(self.smoothed_rtt, self.peak_bandwidth_estimate) / 2;
                    target_cwin = target_cwin.max(min_win);
                }
            }
        } else {
            let margin = us_of_ms(self.params.margin);
            let delta_rtt_target =
                if self.nominal_max_rtt < 4 * margin { self.nominal_max_rtt / 4 } else { margin };
            target_cwin = target_cwin.saturating_add(bytes_from_rate(delta_rtt_target, pacing_rate));
            if matches!(self.alg_state, AlgState::Probing | AlgState::Pushing) {
                let delta_alpha = self.alpha_1024_current.saturating_sub(1024);
                let delta_rate = mult1024(delta_alpha, self.nominal_rate);
                let delta_cwin = bytes_from_rate(self.nominal_max_rtt, delta_rate);
                if delta_cwin < self.mtu {
                    target_cwin = target_cwin.saturating_add(self.mtu - delta_cwin);
                }
            }
        }
        self.cwin = target_cwin;
        self.pacing_rate = pacing_rate;
    }

    // c4.c: c4_growth_evaluate()
    fn growth_evaluate(&self) -> Growth {
        if self.push_alpha > ALPHA_PUSH_LOW {
            // A push large enough to measure growth.
            let old = self.push_rate_old;
            let pushed = mult1024(self.push_alpha, old);
            let target_rate = old.saturating_mul(3).saturating_add(pushed) / 4;
            if self.nominal_rate >= target_rate {
                let higher_rate = old.saturating_add(pushed.saturating_mul(3)) / 4;
                if self.nominal_rate > higher_rate { Growth::Fast } else { Growth::Slow }
            } else {
                Growth::None
            }
        } else if self.nominal_rate > self.push_rate_old && !self.congestion_notified {
            // Too small to measure: decided by the congestion signals.
            Growth::Fast
        } else {
            Growth::None
        }
    }

    // c4.c: c4_growth_reset()
    fn growth_reset(&mut self) {
        self.congestion_notified = false;
        self.push_was_not_limited = false;
        self.push_rate_old = self.nominal_rate;
        self.push_alpha = self.alpha_1024_current;
    }

    // c4.c: c4_era_check(), cc_common.c: picoquic_cc_get_lowest_not_ack(). The era ends once a packet
    // sent in it is acknowledged; quinn's largest acknowledged number stands for picoquic's lowest
    // packet neither acknowledged nor lost.
    fn era_check(&self) -> bool {
        self.highest_acknowledged.is_some_and(|h| h >= self.era_sequence)
    }

    // c4.c: c4_era_reset()
    fn era_reset(&mut self) {
        self.era_sequence = self.send_sequence;
        self.era_max_rtt = 0;
        self.era_min_rtt = u64::MAX;
        self.alpha_1024_previous = self.alpha_1024_current;
        self.update_ecn_alpha();
    }

    // c4.c: c4_enter_initial()
    fn enter_initial(&mut self) {
        self.alg_state = AlgState::Initial;
        self.nominal_max_rtt = self.smoothed_rtt;
        self.initial_cwnd = if self.nominal_rate > 0 {
            bytes_from_rate(self.nominal_max_rtt, self.nominal_rate)
        } else {
            self.cwin
        };
        self.initial_last_time = 0;
        self.probe_level = PROBE_LEVEL_DEFAULT;
        self.alpha_1024_current = ALPHA_INITIAL;
        self.nb_packets_in_startup = 0;
        self.era_reset();
        self.nb_eras_no_increase = 0;
        self.ecn_alpha = 0;
        self.growth_reset();
    }

    // c4.c: c4_exit_initial()
    fn exit_initial(&mut self) {
        if self.nominal_rate > 0 {
            let ssthresh = self.initial_cwnd / 2;
            self.backlog_1024 = 1024;
            self.nominal_max_rtt = rate_from_bytes(ssthresh, self.nominal_rate);
            // The port's guard: no more than C4 accepts at an era's end (running min RTT plus the max
            // jitter), as an app-limited Initial can grow its window far past what it measured.
            self.nominal_max_rtt =
                self.nominal_max_rtt.min(self.running_min_rtt.saturating_add(us_of_ms(self.params.jitter_max)));
            if self.nominal_max_rtt < MAX_RTT_MIN {
                self.nominal_max_rtt = MAX_RTT_MIN;
                self.backlog_1024 = 0;
            } else if self.smoothed_rtt < MAX_RTT_MIN {
                self.backlog_1024 = 0;
            }
            self.delay_threshold = self.compute_delay_threshold();
            self.nb_eras_no_increase = 0;
            self.probe_level = PROBE_LEVEL_DEFAULT;
            self.enter_recovery(Signal::None);
        }
    }

    // c4.c: c4_initial_handle_rtt_excess()
    fn initial_handle_rtt_excess(&mut self) {
        if self.recent_delay_excess > 0 && self.nb_eras_no_increase > 1 && self.push_rate_old >= self.nominal_rate {
            self.exit_initial();
        }
    }

    // c4.c: c4_initial_handle_loss()
    fn initial_handle_loss(&mut self) {
        self.nb_packets_in_startup += 1;
        if self.nb_packets_in_startup > NB_PACKETS_BEFORE_LOSS {
            self.exit_initial();
        }
    }

    // c4.c: c4_initial_handle_ack(). Reno's increase, slowed once the rate stops growing.
    fn initial_handle_ack(&mut self, bytes_acknowledged: u64) {
        self.nb_packets_in_startup += 1;
        let increase = bytes_acknowledged.checked_shr(3 * self.nb_eras_no_increase).unwrap_or(0);
        self.initial_cwnd = self.initial_cwnd.saturating_add(increase);
        if self.era_check() {
            // Eras when the application held the rate back do not count against it.
            let growth = self.growth_evaluate();
            if growth != Growth::None {
                self.nb_eras_no_increase = 0;
            } else if self.push_was_not_limited && self.nominal_rate > 0 {
                self.nb_eras_no_increase += 1;
            }
            self.era_reset();
            if self.nb_eras_no_increase >= 3 {
                self.exit_initial();
            } else {
                self.growth_reset();
            }
        }
    }

    // c4.c: c4_enter_recovery(). Further signals during a recovery do not restart it.
    fn enter_recovery(&mut self, mode: Signal) {
        if self.alg_state == AlgState::Initial {
            self.growth_reset();
        }
        if self.alg_state != AlgState::Recovery {
            let recover = q10(self.params.recover);
            if self.backlog_1024 <= 1024u64.saturating_sub(recover) || self.nominal_max_rtt <= MAX_RTT_MIN {
                self.alpha_1024_current = recover;
            } else if self.backlog_1024 <= 1024 - ALPHA_DRAINING_MID {
                self.alpha_1024_current = ALPHA_DRAINING_MID;
            } else {
                self.alpha_1024_current = ALPHA_DRAINING;
            }
            self.excess_ce_after_push = mode == Signal::Ecn;
            self.alg_state = AlgState::Recovery;
            self.era_reset();
        }
    }

    // c4.c: c4_exit_recovery()
    fn exit_recovery(&mut self, current_time: u64) {
        let growing = self.growth_evaluate() != Growth::None;
        if self.initial_last_time == 0 {
            self.initial_last_time = current_time;
        }
        // What is left of the backlog after an era at alpha.
        let drained = 1024u64.saturating_sub(self.alpha_1024_current);
        self.backlog_1024 = self.backlog_1024.saturating_sub(drained);
        if growing {
            if !self.excess_ce_after_push {
                self.probe_level += 1;
            }
            self.recent_congestions = 0;
            self.previous_maximum_rate = self.recent_maximum_rate;
        } else {
            if self.push_was_not_limited {
                self.probe_level = if self.excess_ce_after_push { 0 } else { 1 };
            }
            if self.congestion_notified {
                // Persistent congestion: back to the largest rate measured since the last recovery.
                self.recent_congestions += 1;
                if self.recent_congestions >= 2
                    && self.recent_maximum_rate > 0
                    && self.nominal_rate > self.recent_maximum_rate
                {
                    self.nominal_rate = self.recent_maximum_rate;
                    self.recent_congestions = 0;
                    self.floor_rate();
                }
                self.previous_maximum_rate = self.recent_maximum_rate;
            } else {
                self.recent_congestions = 0;
                if self.push_was_not_limited {
                    // Trimming: no growth, no congestion, not app-limited.
                    let max_of_2_rates = self.recent_maximum_rate.max(self.previous_maximum_rate);
                    if self.nominal_rate > max_of_2_rates && self.smoothed_rtt > MAX_RTT_MIN {
                        self.nominal_rate = max_of_2_rates;
                        self.floor_rate();
                    }
                    self.previous_maximum_rate = self.recent_maximum_rate;
                }
            }
        }
        self.recent_maximum_rate = 0;
        self.growth_reset();
        self.recent_delay_excess = 0;
        self.smoothed_drop_rate = 0.0;
        self.ecn_alpha = 0;
        if self.probe_level > PROBE_LEVEL_MAX {
            self.enter_initial();
        } else if self.probe_level > PROBE_LEVEL_DEFAULT {
            self.enter_pushing();
        } else {
            self.enter_cruise();
        }
    }

    // c4.c: c4_enter_cruise()
    fn enter_cruise(&mut self) {
        self.era_reset();
        if self.nb_cruise_left_before_push == 0 {
            self.nb_cruise_left_before_push = if self.probe_level == 0 { 1 } else { NB_CRUISE_BEFORE_PUSH };
        }
        self.alpha_1024_current = ALPHA_CRUISE;
        if self.smoothed_rtt < MAX_RTT_MIN {
            // c4.c loosens pacing on sub-millisecond paths, where a busy CPU sends late.
            self.alpha_1024_current += 48;
        }
        self.alg_state = AlgState::Cruising;
    }

    /// c4.c: c4_push_rate_by_probe_level[]
    fn push_rate_by_probe_level(&self, level: u32) -> u64 {
        match level {
            0 => ALPHA_PUSH_VERY_LOW,
            1 => q10(self.params.probe),
            2 => ALPHA_PUSH_50,
            _ => ALPHA_PUSH_200,
        }
    }

    // c4.c: c4_enter_probing()
    fn enter_probing(&mut self) {
        self.alpha_1024_current = self.push_rate_by_probe_level(self.probe_level);
        self.push_alpha = self.alpha_1024_current;
        self.era_reset();
        self.alg_state = AlgState::Probing;
    }

    // c4.c: c4_enter_pushing(). Its push_limit is never read in c4.c and is left out.
    fn enter_pushing(&mut self) {
        self.alpha_1024_previous = self.alpha_1024_current;
        self.alpha_1024_current = q10(self.params.push);
        self.push_alpha = self.alpha_1024_current;
        self.alg_state = AlgState::Pushing;
        self.nb_eras_no_increase = 0;
        self.growth_reset();
        self.era_reset();
    }

    // c4.c: c4_on_pushing_rate_measurement()
    fn on_pushing_rate_measurement(&mut self) {
        if self.growth_evaluate() == Growth::Fast {
            self.growth_reset();
            self.era_reset();
            self.nb_eras_no_increase = 0;
        }
    }

    // c4.c: c4_on_pushing_era_end()
    fn on_pushing_era_end(&mut self) {
        if self.growth_evaluate() == Growth::Fast {
            self.growth_reset();
            self.era_reset();
        } else if self.alpha_1024_previous >= q10(self.params.push) {
            // The push may have queued up to its excess over the rate: count it as backlog.
            self.backlog_1024 = self.backlog_1024.saturating_add(self.alpha_1024_current.saturating_sub(1024));
            self.enter_recovery(Signal::None);
        }
    }

    // c4.c: c4_update_min_max_rtt(). The max RTT learns only from eras sent at most at the nominal
    // rate (alpha_previous <= 1).
    fn update_min_max_rtt(&mut self) {
        let sample = self.rtt_sample;
        self.era_max_rtt = self.era_max_rtt.max(sample);
        self.era_min_rtt = self.era_min_rtt.min(sample);
        if self.alpha_1024_previous <= ALPHA_NEUTRAL {
            if self.era_min_rtt < self.running_min_rtt {
                self.running_min_rtt = self.era_min_rtt;
            } else {
                self.running_min_rtt = ((7 * self.running_min_rtt as u128 + self.era_min_rtt as u128) / 8) as u64;
            }
            let corrected_max =
                self.era_max_rtt.min(self.running_min_rtt.saturating_add(us_of_ms(self.params.jitter_max)));
            if corrected_max > self.nominal_max_rtt {
                self.nominal_max_rtt = corrected_max;
            } else {
                self.nominal_max_rtt = ((7 * self.nominal_max_rtt as u128 + corrected_max as u128) / 8) as u64;
            }
            self.delay_threshold = self.compute_delay_threshold();
        } else if self.nominal_max_rtt == 0 {
            self.nominal_max_rtt = self.era_max_rtt;
            self.delay_threshold = self.compute_delay_threshold();
        }
        if self.nominal_max_rtt < MAX_RTT_MIN {
            self.backlog_1024 = 0;
            self.nominal_max_rtt = MAX_RTT_MIN;
            self.delay_threshold = self.compute_delay_threshold();
        }
    }

    // c4.c: c4_handle_ack()
    fn handle_ack(&mut self, ack: Ack) {
        let previous_rate = self.nominal_rate;
        if ack.rtt > 0 && ack.delivered_since > 0 {
            let rate_measurement = self.bandwidth_estimate;
            self.recent_maximum_rate = self.recent_maximum_rate.max(rate_measurement);
            // No raise in a congested recovery: packets sent at the old rate would bounce it back.
            if rate_measurement > self.nominal_rate
                && !(self.alg_state == AlgState::Recovery && self.congestion_notified)
            {
                self.push_was_not_limited = true;
                self.nominal_rate = rate_measurement;
                self.delay_threshold = self.compute_delay_threshold();
                self.backlog_1024 = 0;
            } else {
                // More delivered than a min RTT at the rate: the network held it back, not the sender.
                let target_cwin = bytes_from_rate(self.running_min_rtt, previous_rate);
                if ack.delivered_since > target_cwin {
                    self.push_was_not_limited = true;
                }
            }
        }
        if self.alg_state == AlgState::Initial {
            self.initial_handle_ack(ack.bytes);
        } else if self.era_check() {
            self.update_min_max_rtt();
            // A max RTT far over the min RTT on a slow, long path: the Initial may have ended early
            // on jitter, so it starts again (at most once a second after the last one).
            if self.nominal_max_rtt > 50_000
                && self.nominal_rate < 1_000_000
                && self.running_min_rtt.saturating_mul(5) < self.nominal_max_rtt.saturating_mul(2)
                && self.initial_last_time > 0
                && ack.now > self.initial_last_time.saturating_add(1_000_000)
            {
                self.enter_initial();
            } else {
                match self.alg_state {
                    AlgState::Recovery => self.exit_recovery(ack.now),
                    AlgState::Cruising => {
                        if self.nb_cruise_left_before_push > 0 {
                            self.nb_cruise_left_before_push -= 1;
                        }
                        self.era_reset();
                        if self.nb_cruise_left_before_push == 0
                            && self.last_time_acked_data_frame_sent > self.last_sender_limited_time
                        {
                            self.enter_probing();
                        }
                    }
                    AlgState::Probing => self.enter_recovery(Signal::None),
                    AlgState::Pushing => self.on_pushing_era_end(),
                    AlgState::Initial => self.era_reset(),
                }
            }
        } else if self.alg_state == AlgState::Pushing {
            self.on_pushing_rate_measurement();
        }
    }

    // c4.c: c4_notify_congestion()
    fn notify_congestion(&mut self, mode: Signal) {
        let beta_max = q10(self.params.beta);
        let mut beta = beta_max;
        self.congestion_notified = true;
        if mode == Signal::Loss {
            // Faster flows slow down more, for fairness between C4 flows.
            beta = (beta_max + mult1024(self.sensitivity_1024(), beta_max)) / 2;
        } else if mode == Signal::Ecn {
            beta = (self.ecn_alpha.saturating_sub(self.ecn_threshold) * 1024 / self.ecn_threshold.max(1)).min(beta_max);
            self.backlog_1024 = 0;
        }
        if mode == Signal::Delay {
            beta = if self.delay_threshold > 0 {
                (self.recent_delay_excess.saturating_mul(1024) / self.delay_threshold).min(beta_max)
            } else {
                beta_max
            };
        } else {
            self.recent_delay_excess = 0;
        }
        if self.alg_state == AlgState::Recovery {
            if self.alpha_1024_current == q10(self.params.recover) {
                // Queues still building after entering recovery: drain more, a round trip longer.
                self.alpha_1024_current = ALPHA_RECOVER2;
                self.era_sequence = self.send_sequence;
            }
            if mode == Signal::Ecn {
                self.excess_ce_after_push = true;
            }
        } else {
            // Probing and pushing sent over the nominal rate: their signals do not cut it.
            if self.alg_state != AlgState::Probing && self.alg_state != AlgState::Pushing {
                self.nominal_rate -= mult1024(beta, self.nominal_rate);
                if mode == Signal::Loss {
                    self.nominal_max_rtt -= mult1024(beta, self.nominal_max_rtt);
                    self.nominal_max_rtt = self.nominal_max_rtt.max(MAX_RTT_MIN);
                    self.delay_threshold = self.compute_delay_threshold();
                }
                self.floor_rate();
            }
            self.enter_recovery(mode);
        }
        self.apply_rate_and_cwin();
    }

    /// The min_rate parameter, applied where c4.c cuts the nominal rate.
    fn floor_rate(&mut self) {
        let min_rate = self.params.min_rate as u64;
        if min_rate > 0 && self.nominal_rate < min_rate {
            self.nominal_rate = min_rate;
        }
    }

    // c4.c: c4_update_rtt()
    fn update_rtt(&mut self, rtt_measurement: u64) {
        self.era_max_rtt = self.era_max_rtt.max(rtt_measurement);
        self.era_min_rtt = self.era_min_rtt.min(rtt_measurement);
        self.running_min_rtt = self.running_min_rtt.min(rtt_measurement);
        if self.nominal_max_rtt == 0 {
            self.nominal_max_rtt = rtt_measurement.max(MAX_RTT_MIN);
            self.delay_threshold = self.compute_delay_threshold();
            self.recent_delay_excess = 0;
        } else {
            let target_rtt = self.nominal_max_rtt.saturating_add(self.delay_threshold);
            self.recent_delay_excess = rtt_measurement.saturating_sub(target_rtt);
        }
    }

    // c4.c: c4_handle_rtt_excess()
    fn handle_rtt_excess(&mut self) {
        if self.recent_delay_excess > 0 && self.alpha_1024_previous > 1024 {
            self.notify_congestion(Signal::Delay);
        }
    }

    // frames.c: picoquic_estimate_path_bandwidth(). The delivery rate since the reference stamped
    // on the packet when sent, over the longer of the receive and send intervals.
    fn estimate_path_bandwidth(&mut self, send_time: u64, prior: &Sent, delivery_time: u64) {
        if send_time >= self.delivered_sent_last {
            if self.delivered_time_last == 0 {
                self.delivered_last = self.delivered;
                self.delivered_time_last = delivery_time;
                self.delivered_sent_last = send_time;
            } else {
                let mut receive_interval = delivery_time.saturating_sub(prior.delivered_time);
                if receive_interval > BANDWIDTH_TIME_INTERVAL_MIN {
                    let delivered = self.delivered.saturating_sub(prior.delivered);
                    let send_interval = send_time.saturating_sub(prior.delivered_sent);
                    receive_interval = receive_interval.max(send_interval);
                    self.bandwidth_estimate = rate_from_bytes(delivered, receive_interval);
                    self.delivered_last = self.delivered;
                    self.delivered_time_last = delivery_time;
                    self.delivered_sent_last = send_time;
                }
            }
        }
    }

    // frames.c: picoquic_estimate_max_path_bandwidth()
    fn estimate_max_path_bandwidth(&mut self, send_time: u64, delivery_time: u64) {
        if send_time >= self.max_sample_sent_time {
            if self.max_sample_sent_time == 0 {
                self.max_sample_delivered = self.delivered;
                self.max_sample_acked_time = delivery_time;
                self.max_sample_sent_time = send_time;
            } else {
                let mut receive_interval = delivery_time.saturating_sub(self.max_sample_acked_time);
                if receive_interval > BANDWIDTH_TIME_INTERVAL_MIN {
                    let delivered = self.delivered.saturating_sub(self.max_sample_delivered);
                    let send_interval = send_time.saturating_sub(self.max_sample_sent_time);
                    receive_interval = receive_interval.max(send_interval);
                    self.peak_bandwidth_estimate =
                        self.peak_bandwidth_estimate.max(rate_from_bytes(delivered, receive_interval));
                    self.max_sample_delivered = self.delivered;
                    self.max_sample_acked_time = delivery_time;
                    self.max_sample_sent_time = send_time;
                }
            }
        }
    }

    /// The last transmit sent at or before a time.
    fn find(&self, time: u64) -> Option<Sent> {
        let i = self.sent.partition_point(|s| s.time <= time);
        if i == 0 { None } else { self.sent.get(i - 1).copied() }
    }

    /// Runs the acknowledgement notification of the last ACK frame, if still waiting.
    fn flush(&mut self) {
        if let Some(ack) = self.pending.take() {
            self.handle_ack(ack);
            self.apply_rate_and_cwin();
        }
    }

    /// One acknowledged packet (Controller::on_ack, with quinn's smoothed RTT).
    fn ack(&mut self, now: Instant, sent: Instant, bytes: u64, srtt: Duration) {
        self.flush();
        let now = self.time(now);
        let sent = self.time(sent).min(now);
        self.smoothed_rtt = srtt.as_micros().clamp(1, 60_000_000) as u64;
        self.delivered = self.delivered.saturating_add(bytes);
        self.ecn_acked_total += 1;
        self.batch_bytes = self.batch_bytes.saturating_add(bytes);
        self.batch_count += 1;
        self.batch_newest = self.batch_newest.max(sent);
        self.last_time_acked_data_frame_sent = self.last_time_acked_data_frame_sent.max(sent);
    }

    /// The end of an ACK frame (Controller::on_end_acks). Its RTT sample is the newest packet's, as
    /// picoquic takes the largest acknowledged; quinn's ACK delay is not available and stays in.
    fn end_acks(&mut self, now: Instant, app_limited: bool, largest: Option<u64>) {
        self.flush();
        let now = self.time(now);
        if app_limited {
            // Stands for picoquic's last_sender_limited_time: the sender ran out of data before now.
            self.last_sender_limited_time = now;
        }
        if let Some(largest) = largest {
            self.highest_acknowledged = Some(self.highest_acknowledged.map_or(largest, |h| h.max(largest)));
        }
        if self.batch_count == 0 {
            return;
        }
        let (bytes, newest) = (self.batch_bytes, self.batch_newest);
        self.batch_bytes = 0;
        self.batch_count = 0;
        self.batch_newest = 0;
        let rtt = now.saturating_sub(newest);
        if rtt == 0 {
            // picoquic notifies no acknowledgement without an RTT sample.
            return;
        }
        self.rtt_sample = rtt;
        // c4.c: c4_notify(picoquic_congestion_notification_rtt_measurement)
        self.update_rtt(rtt);
        if self.alg_state == AlgState::Initial {
            self.initial_handle_rtt_excess();
            self.apply_rate_and_cwin();
        } else {
            self.handle_rtt_excess();
        }
        // frames.c: process_decoded_packet_data()
        let delivered_since = match self.find(newest) {
            Some(prior) => {
                self.estimate_path_bandwidth(newest, &prior, now);
                self.delivered.saturating_sub(prior.delivered)
            }
            None => 0,
        };
        self.estimate_max_path_bandwidth(newest, now);
        self.pending = Some(Ack { now, bytes, rtt, delivered_since });
    }

    // c4.c: c4_notify(picoquic_congestion_notification_repeat), once per lost packet.
    fn repeat(&mut self, lost_packet_number: u64) {
        if self.alg_state == AlgState::Recovery && lost_packet_number < self.era_sequence {
            // Sent before the recovery: its loss was answered by entering it.
            return;
        }
        self.update_loss_rate(lost_packet_number);
        if self.smoothed_drop_rate > self.loss_threshold() {
            if self.alg_state == AlgState::Initial {
                self.initial_handle_loss();
            } else {
                self.notify_congestion(Signal::Loss);
            }
        }
    }

    /// A quinn loss event: its bytes, as packets of its transmit's average size, numbered down
    /// from the largest lost packet's transmit.
    fn lost(&mut self, sent: u64, lost_bytes: u64) {
        let (largest, size) = match self.find(sent) {
            Some(s) => {
                let packets = s.last.saturating_sub(s.first).saturating_add(1).clamp(1, MAX_LOST_PACKETS);
                (s.last, s.bytes / packets)
            }
            // Older than what is remembered: replayed as packet 0, which does not raise the loss rate.
            None => (0, self.mtu),
        };
        let size = size.max(1);
        let packets = (lost_bytes.saturating_add(size - 1) / size).clamp(1, MAX_LOST_PACKETS);
        for i in (0..packets).rev() {
            self.repeat(largest.saturating_sub(i));
        }
    }

    // c4.c: c4_notify(picoquic_congestion_notification_ecn_ec)
    fn ecn(&mut self) {
        self.ecn_ce_total += 1;
        self.ecn_threshold = self.compute_ecn_threshold();
        self.update_ecn_alpha();
        if self.ecn_alpha > self.ecn_threshold {
            if self.alg_state == AlgState::Initial {
                // The same test as for delay: the rate stopped growing two eras ago.
                self.initial_handle_rtt_excess();
            } else {
                self.notify_congestion(Signal::Ecn);
            }
        }
    }
}

impl Controller for C4 {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.flush();
        // Kept in time order for find(), whatever the clock does.
        let time = self.time(now).max(self.sent.back().map_or(0, |s| s.time));
        // A smaller number than before is a new packet number space (handshake): a batch of one.
        let first = if last_packet_number >= self.send_sequence { self.send_sequence } else { last_packet_number };
        if self.sent.len() >= SENT_RECORDS {
            self.sent.pop_front();
        }
        self.sent.push_back(Sent {
            first,
            last: last_packet_number,
            time,
            bytes,
            delivered: self.delivered_last,
            delivered_time: self.delivered_time_last,
            delivered_sent: self.delivered_sent_last,
        });
        self.send_sequence = self.send_sequence.max(last_packet_number.saturating_add(1));
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, _app_limited: bool, rtt: &RttEstimator) {
        self.ack(now, sent, bytes, rtt.get());
    }

    fn on_end_acks(&mut self, now: Instant, _in_flight: u64, app_limited: bool, largest_packet_num_acked: Option<u64>) {
        self.end_acks(now, app_limited, largest_packet_num_acked);
    }

    // C4 has no persistent-congestion collapse (a Wi-Fi blackout is not congestion): the flag is
    // not used; the lost packets count like any others.
    fn on_congestion_event(&mut self, now: Instant, sent: Instant, _is_persistent_congestion: bool, lost_bytes: u64) {
        let now = self.time(now);
        if self.pending.is_some_and(|ack| ack.now != now) {
            self.flush();
        }
        if lost_bytes > 0 {
            let sent = self.time(sent);
            self.lost(sent, lost_bytes);
        } else {
            self.ecn();
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.flush();
        self.mtu = u64::from(new_mtu.max(1));
        self.apply_rate_and_cwin();
    }

    fn window(&self) -> u64 {
        let mut window = self.cwin;
        if self.params.wcap > 0.0 && self.pacing_rate > 0 {
            let cap = self.params.wcap * self.pacing_rate as f64 * self.smoothed_rtt as f64 / 1e6;
            window = window.min(cap as u64);
        }
        window.clamp((self.params.min_win as u64).max(2) * self.mtu, MAX_WINDOW)
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = ControllerMetrics::default();
        metrics.congestion_window = self.window();
        metrics.pacing_rate = (self.pacing_rate > 0).then(|| self.pacing_rate.saturating_mul(8));
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.cwin_initial()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl Report for C4 {
    /// The nominal rate, times alpha while that is under 1 (a recovery draining a queue), or 1 Mbit/s
    /// before any measurement; at most the window over the smoothed RTT.
    fn budget(&self, srtt: Duration) -> f64 {
        let rate: u64 = if self.nominal_rate == 0 {
            INITIAL_PACING
        } else {
            mult1024(self.alpha_1024_current.min(ALPHA_NEUTRAL), self.nominal_rate)
        };
        let srtt = srtt.as_secs_f64();
        if srtt > 0.0 { (rate as f64).min(self.window() as f64 / srtt) } else { rate as f64 }
    }

    fn state(&self) -> String {
        let state = match self.alg_state {
            AlgState::Initial => "Initial",
            AlgState::Recovery if self.congestion_notified => "Recovery (congested)",
            AlgState::Recovery => "Recovery",
            AlgState::Cruising => "Cruising",
            AlgState::Probing => "Probing",
            AlgState::Pushing => "Pushing",
        };
        state.to_string()
    }

    /// The last RTT sample over the running min RTT.
    fn queue(&self) -> Option<Duration> {
        if self.rtt_sample == 0 || self.running_min_rtt == u64::MAX {
            return None;
        }
        Some(Duration::from_micros(self.rtt_sample.saturating_sub(self.running_min_rtt)))
    }

    /// The RTT above which delay counts as congestion (max RTT plus delay threshold) and the last
    /// sample's excess over it, in milliseconds.
    fn trend(&self) -> (f64, f64) {
        let target = self.nominal_max_rtt.saturating_add(self.delay_threshold);
        (target as f64 / 1e3, self.recent_delay_excess as f64 / 1e3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: u64 = 1200;

    /// One bottleneck of rate bytes a second (drop-tail, 2 s of buffer) and a fixed round trip
    /// besides; the sender sends what quinn would: within the window, then a 1.25 window / srtt
    /// pacer. quinn updates its smoothed RTT after the acknowledgements, from the newest packet.
    struct Sim {
        c4: C4,
        now: Instant,
        rate: f64,
        rtt: Duration,
        free_at: Instant,
        /// Packet number, sent, when its acknowledgement arrives (None: dropped).
        flight: VecDeque<(u64, Instant, Option<Instant>)>,
        in_flight: u64,
        pn: u64,
        tokens: f64,
        srtt: Option<Duration>,
        /// What the application makes, bytes a second; None: all it may.
        app: Option<f64>,
        queued: f64,
        app_limited: bool,
    }

    impl Sim {
        fn new(rate: f64, rtt_ms: u64, app: Option<f64>) -> Sim {
            let now = Instant::now();
            Sim {
                c4: C4::new(now, MTU as u16, C4Params::default()),
                now,
                rate,
                rtt: Duration::from_millis(rtt_ms),
                free_at: now,
                flight: VecDeque::new(),
                in_flight: 0,
                pn: 0,
                tokens: (10 * MTU) as f64,
                srtt: None,
                app,
                queued: 0.0,
                app_limited: false,
            }
        }

        fn run(&mut self, seconds: f64) {
            let step = Duration::from_micros(250);
            for _ in 0..(seconds / step.as_secs_f64()) as usize {
                self.step(step);
            }
        }

        fn step(&mut self, step: Duration) {
            self.now += step;
            let now = self.now;
            let mut acked = Vec::new();
            let mut i = 0;
            while i < self.flight.len() {
                let (pn, sent, at) = self.flight[i];
                match at {
                    Some(at) if at <= now => {
                        acked.push((pn, sent));
                        self.flight.remove(i);
                    }
                    Some(_) => break,
                    None => i += 1,
                }
            }
            if let Some(&(largest, newest)) = acked.last() {
                let srtt = self.srtt.unwrap_or(Duration::from_millis(333));
                for &(_, sent) in &acked {
                    self.c4.ack(now, sent, MTU, srtt);
                }
                self.in_flight -= MTU * acked.len() as u64;
                self.c4.end_acks(now, self.app_limited, Some(largest));
                let sample = now - newest;
                self.srtt = Some(self.srtt.map_or(sample, |s| (s * 7 + sample) / 8));
                let (mut lost, mut lost_sent) = (0, None);
                self.flight.retain(|&(pn, sent, at)| {
                    let gone = at.is_none() && pn + 3 <= largest;
                    if gone {
                        lost += MTU;
                        lost_sent = Some(sent);
                    }
                    !gone
                });
                if let Some(sent) = lost_sent {
                    self.in_flight -= lost;
                    self.c4.on_congestion_event(now, sent, false, lost);
                }
            }
            if let Some(rate) = self.app {
                self.queued += rate * step.as_secs_f64();
            }
            let srtt = self.srtt.unwrap_or(Duration::from_millis(333)).as_secs_f64();
            let window = self.c4.window() as f64;
            let capacity = (window * 0.002 / srtt).clamp(10.0 * MTU as f64, 256.0 * MTU as f64);
            self.tokens = (self.tokens + 1.25 * window * step.as_secs_f64() / srtt).min(capacity);
            let mut blocked = false;
            loop {
                if self.app.is_some() && self.queued < MTU as f64 {
                    break;
                }
                if self.in_flight + MTU >= self.c4.window() || self.tokens < MTU as f64 {
                    blocked = true;
                    break;
                }
                self.tokens -= MTU as f64;
                if self.app.is_some() {
                    self.queued -= MTU as f64;
                }
                let start = self.free_at.max(now);
                let at = if start - now > Duration::from_secs(2) {
                    None
                } else {
                    self.free_at = start + Duration::from_secs_f64(MTU as f64 / self.rate);
                    Some(self.free_at + self.rtt)
                };
                self.flight.push_back((self.pn, now, at));
                self.in_flight += MTU;
                self.c4.on_sent(now, MTU, self.pn);
                self.pn += 1;
            }
            self.app_limited = !blocked && self.app.is_some();
        }
    }

    /// C4 just out of its Initial at 2.5 MB/s on a 20 ms path (max RTT = initial_cwnd / 2 / rate).
    fn after_initial(start: Instant, params: C4Params) -> C4 {
        let mut c4 = C4::new(start, MTU as u16, params);
        c4.nominal_rate = 2_500_000;
        c4.initial_cwnd = 100_000;
        c4.running_min_rtt = 20_000;
        c4.smoothed_rtt = 20_000;
        c4.exit_initial();
        c4.apply_rate_and_cwin();
        c4
    }

    /// Sends one packet at t, acknowledged rtt_ms later (srtt 20 ms, not app-limited), lets the
    /// acknowledgement take effect and moves t 5 ms past it; returns the send time.
    fn round(c4: &mut C4, t: &mut Instant, rtt_ms: u64) -> Instant {
        let sent = *t;
        let pn = c4.send_sequence;
        c4.on_sent(sent, MTU, pn);
        let at = sent + Duration::from_millis(rtt_ms);
        c4.ack(at, sent, MTU, Duration::from_millis(20));
        c4.end_acks(at, false, Some(pn));
        c4.flush();
        *t = at + Duration::from_millis(5);
        sent
    }

    #[test]
    fn grows_on_a_clean_path() {
        let mut sim = Sim::new(2_500_000.0, 20, None);
        let first = sim.c4.window();
        sim.run(3.0);
        assert!(sim.c4.nominal_rate > 1_000_000, "nominal rate {}", sim.c4.nominal_rate);
        assert!(sim.c4.nominal_rate < 5_000_000, "nominal rate {}", sim.c4.nominal_rate);
        assert!(sim.c4.window() > 4 * first, "window {} from {first}", sim.c4.window());
    }

    /// C4 acts on delay only while probing or pushing: it leaves for Recovery (the window falls
    /// from 17/16 to 3/4 of the rate here, a backlog being left from the Initial) without cutting
    /// the rate; the second congested recovery in a row sets the rate to the largest measured since
    /// the first (48 kB/s: one packet every 25 ms).
    #[test]
    fn delay_while_probing_cuts_window_then_rate() {
        let start = Instant::now();
        let mut c4 = after_initial(start, C4Params::default());
        let mut t = start + Duration::from_millis(1);
        round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Cruising);
        let cruising = c4.window();
        assert_eq!(cruising, 62_500);
        round(&mut c4, &mut t, 20);
        round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Probing);
        let probing = c4.window();
        round(&mut c4, &mut t, 50);
        assert_eq!(c4.alg_state, AlgState::Recovery);
        assert_eq!(c4.state(), "Recovery (congested)");
        assert_eq!(c4.nominal_rate, 2_500_000);
        assert!(c4.window() < probing, "window {} probing {probing}", c4.window());
        round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Cruising);
        assert_eq!(c4.nominal_rate, 2_500_000);
        round(&mut c4, &mut t, 20);
        round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Probing);
        round(&mut c4, &mut t, 50);
        assert_eq!(c4.alg_state, AlgState::Recovery);
        round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Cruising);
        assert_eq!(c4.nominal_rate, 48_000);
        assert!(c4.window() < cruising, "window {} cruising {cruising}", c4.window());
        assert!(c4.budget(Duration::from_millis(20)) <= 48_000.0);
    }

    /// As c4.c: one loss after a clean stretch gives a smoothed loss rate of 1/16, over the 4.5 %
    /// threshold at 2.5 MB/s (sensitivity 973/1024); cruising, the rate and the max RTT are cut by
    /// beta = (1/4 + sensitivity / 4) / 2 = 249/1024 and C4 enters Recovery. Losses of packets sent
    /// before it are ignored; later ones in the same recovery cut nothing more.
    #[test]
    fn loss_cuts_rate_and_max_rtt_once() {
        let start = Instant::now();
        let mut c4 = after_initial(start, C4Params::default());
        let mut t = start + Duration::from_millis(1);
        let first = round(&mut c4, &mut t, 20);
        let second = round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Cruising);
        assert_eq!(c4.sensitivity_1024(), 973);
        let beta: u64 = (256 + (973 * 256 >> 10)) / 2;
        assert_eq!(beta, 249);
        c4.on_congestion_event(t, second, false, MTU);
        assert_eq!(c4.alg_state, AlgState::Recovery);
        assert_eq!(c4.nominal_rate, 2_500_000 - (2_500_000 * beta >> 10));
        assert_eq!(c4.nominal_max_rtt, 20_000 - (20_000 * beta >> 10));
        let rate = c4.nominal_rate;
        c4.on_congestion_event(t, first, false, MTU);
        assert_eq!(c4.nominal_rate, rate);
        c4.on_sent(t, MTU, c4.send_sequence);
        c4.on_congestion_event(t + Duration::from_millis(30), t, false, MTU);
        assert_eq!(c4.nominal_rate, rate);
        assert_eq!(c4.alg_state, AlgState::Recovery);
    }

    #[test]
    fn app_limited_traffic_stays_bounded() {
        let app = 125_000.0;
        let mut sim = Sim::new(2_500_000.0, 20, Some(app));
        sim.run(5.0);
        assert!((sim.c4.nominal_rate as f64) < 3.0 * app, "nominal rate {}", sim.c4.nominal_rate);
        assert!(sim.c4.window() <= 20 * MTU, "window {}", sim.c4.window());
        assert!(sim.c4.budget(Duration::from_millis(20)) < 3.0 * app);
    }

    #[test]
    fn wcap_caps_the_window_at_the_pacing_rate() {
        let start = Instant::now();
        let mut params = C4Params::default();
        params.set("wcap", "1").unwrap();
        let mut c4 = after_initial(start, params);
        let mut t = start + Duration::from_millis(1);
        round(&mut c4, &mut t, 20);
        assert_eq!(c4.alg_state, AlgState::Cruising);
        assert_eq!(c4.window(), 50_000);
    }

    /// Timings no network gives (time going back, sent after now, zero and huge gaps, zero bytes,
    /// packet numbers jumping), with default and extreme parameters: no panic, the window stays
    /// within its floor and quinn's pacing limit, every number stays finite.
    #[test]
    fn odd_timings_keep_it_sane() {
        let mut lo = C4Params::default();
        let mut hi = C4Params::default();
        for (key, low, high) in [
            ("delay_max", "1", "200"),
            ("margin", "0", "100"),
            ("jitter_max", "10", "2000"),
            ("loss_base", "0", "0.5"),
            ("beta", "0.05", "0.5"),
            ("push", "1.03", "2"),
            ("probe", "1", "1.5"),
            ("recover", "0.5", "1"),
            ("min_rate", "0", "12500000"),
            ("min_win", "2", "64"),
            ("wcap", "0.5", "8"),
        ] {
            lo.set(key, low).unwrap();
            hi.set(key, high).unwrap();
        }
        for params in [C4Params::default(), lo, hi] {
            let start = Instant::now();
            let mut c4 = C4::new(start, 1200, params);
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            let mut now = start;
            let mut pn = 0u64;
            for _ in 0..20_000 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let r = seed;
                match r % 16 {
                    0 => now += Duration::from_secs(r % 120),
                    1 => now = now.checked_sub(Duration::from_micros(r % 10_000)).unwrap_or(now),
                    2 => {}
                    _ => now += Duration::from_micros(r % 5_000),
                }
                let sent = if r & 32 == 0 {
                    now.checked_sub(Duration::from_micros(r % 400_000)).unwrap_or(now)
                } else {
                    now + Duration::from_micros(r % 3_000)
                };
                let srtt = Duration::from_micros((r >> 24) % 2_000_000);
                match (r >> 8) % 8 {
                    0..=2 => {
                        pn = match (r >> 16) % 8 {
                            0 => pn + (r >> 20) % 1_000,
                            1 => (r >> 20) % 5,
                            _ => pn + 1 + (r >> 20) % 3,
                        };
                        c4.on_sent(now, (r >> 30) % 70_000, pn);
                    }
                    3 | 4 => c4.ack(now, sent, (r >> 30) % 70_000, srtt),
                    5 => c4.end_acks(now, r & 64 == 0, (r & 128 == 0).then(|| (r >> 33) % (pn + 2))),
                    6 => {
                        let lost = if r % 3 == 0 { 0 } else { (r >> 30) % 100_000 };
                        c4.on_congestion_event(now, sent, r & 64 == 0, lost);
                    }
                    _ => c4.on_mtu_update(1200 + ((r >> 40) % 400) as u16),
                }
                let window = c4.window();
                assert!(window >= 2 * c4.mtu && window <= MAX_WINDOW, "window {window}");
                let budget = c4.budget(srtt);
                assert!(budget.is_finite() && budget >= 0.0, "budget {budget}");
                let (a, b) = c4.trend();
                assert!(a.is_finite() && b.is_finite());
                assert!((0.0..=1.0).contains(&c4.smoothed_drop_rate), "drop rate {}", c4.smoothed_drop_rate);
                assert!(c4.state().chars().all(|c| c.is_ascii_alphabetic() || " ()".contains(c)));
                assert!(c4.sent.len() <= SENT_RECORDS);
            }
        }
    }

    #[test]
    fn params_parse_and_describe() {
        let mut params = C4Params::default();
        assert_eq!(
            params.describe(),
            "delay_max=25 margin=15 jitter_max=250 loss_base=0.02 beta=0.25 push=1.2265625 probe=1.0625 \
             recover=0.9375 min_rate=0 min_win=4 wcap=0"
        );
        params.set("loss_base", "0.07").unwrap();
        assert_eq!(params.loss_base, 0.07);
        params.set("min_win", " 6 ").unwrap();
        assert_eq!(params.min_win, 6.0);
        assert!(params.set("nope", "1").is_err());
        assert!(params.set("beta", "0.9").is_err());
        assert!(params.set("beta", "x").is_err());
        assert!(params.set("delay_max", "NaN").is_err());
        assert!(params.set("min_win", "2.5").is_err());
        assert!(params.set("wcap", "0.2").is_err());
        assert_eq!(params.beta, 0.25);
    }
}
