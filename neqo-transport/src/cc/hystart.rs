// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{
    cmp::{max, min},
    time::Duration,
    usize,
};

use neqo_common::qinfo;

use crate::{
    cc::classic_cc::{SlowStart, SlowStartResult},
    packet,
    rtt::RttEstimate,
};

#[derive(Debug, Default, Clone, Copy, derive_more::Display)]
#[display("State [last_min: {last_round_min_rtt:?}, current_min: {current_round_min_rtt:?}, samples: {rtt_sample_count}, end: {window_end:?}, css: {css_baseline_min_rtt:?}")]
pub struct State {
    last_round_min_rtt: Duration,
    current_round_min_rtt: Duration,
    rtt_sample_count: usize,
    window_end: Option<packet::Number>,
    css_baseline_min_rtt: Duration,
    css_round_count: usize,
}

impl State {
    pub const fn new() -> Self {
        Self {
            last_round_min_rtt: Duration::MAX,
            current_round_min_rtt: Duration::MAX,
            rtt_sample_count: 0,
            window_end: None,
            css_baseline_min_rtt: Duration::MAX,
            css_round_count: 0,
        }
    }
}

#[derive(Debug, Default, derive_more::Display)]
#[display("HyStart++")]
pub struct HyStart {
    limit: usize,
    current: State,
}

impl HyStart {
    pub const MIN_RTT_THRESH: Duration = Duration::from_millis(4);

    pub const MAX_RTT_THRESH: Duration = Duration::from_millis(16);

    pub const MIN_RTT_DIVISOR: u32 = 8;

    pub const N_RTT_SAMPLE: usize = 8;

    pub const CSS_GROWTH_DIVISOR: usize = 4;

    pub const CSS_ROUNDS: usize = 5;

    pub const NON_PACED_L: usize = 8;

    pub const fn new(pacing: bool) -> Self {
        let limit = if pacing {
            usize::MAX
        } else {
            Self::NON_PACED_L
        };
        Self {
            limit,
            current: State::new(),
        }
    }

    pub fn in_css(&self) -> bool {
        self.current.css_baseline_min_rtt != Duration::MAX
    }

    fn collect_rtt_sample(&mut self, rtt: Duration) {
        self.current.current_round_min_rtt = min(self.current.current_round_min_rtt, rtt);
        self.current.rtt_sample_count += 1;
    }

    const fn maybe_exit_css(&mut self) -> bool {
        self.current.css_round_count += 1;
        self.current.css_round_count >= Self::CSS_ROUNDS
    }

    fn calc_cwnd_increase(&self, new_acked: usize, max_datagram_size: usize, css: bool) -> usize {
        let mut cwnd_increase = min(
            self.limit
                .checked_mul(max_datagram_size)
                .unwrap_or(usize::MAX),
            new_acked,
        );

        if css {
            cwnd_increase /= Self::CSS_GROWTH_DIVISOR;
        }
        cwnd_increase
    }

    const fn enough_samples(&self) -> bool {
        self.current.rtt_sample_count >= Self::N_RTT_SAMPLE
    }

    fn maybe_start_new_round(&mut self, sent_pn: packet::Number) {
        if self.current.window_end.is_some() {
            return;
        }
        self.current.window_end = Some(sent_pn);
        self.current.last_round_min_rtt = self.current.current_round_min_rtt;
        self.current.current_round_min_rtt = Duration::MAX;
        self.current.rtt_sample_count = 0;
        qinfo!("started new round");
    }

    #[cfg(test)]
    /// Test accessor: Get window end packet number
    pub const fn window_end(&self) -> Option<packet::Number> {
        self.current.window_end
    }

    #[cfg(test)]
    /// Test accessor: Get RTT sample count for current round
    pub const fn rtt_sample_count(&self) -> usize {
        self.current.rtt_sample_count
    }

    #[cfg(test)]
    /// Test accessor: Get current round minimum RTT
    pub const fn current_round_min_rtt(&self) -> Duration {
        self.current.current_round_min_rtt
    }

    #[cfg(test)]
    /// Test accessor: Get CSS round count
    pub const fn css_round_count(&self) -> usize {
        self.current.css_round_count
    }
}

impl SlowStart for HyStart {
    fn on_packet_sent(&mut self, sent_pn: packet::Number) {
        self.maybe_start_new_round(sent_pn);
    }

    fn on_packets_acked(
        &mut self,
        curr_cwnd: usize,
        ssthresh: usize,
        new_acked: usize,
        rtt_est: &RttEstimate,
        max_datagram_size: usize,
        largest_acked: packet::Number,
    ) -> SlowStartResult {
        debug_assert!(
            ssthresh >= curr_cwnd,
            "ssthresh {ssthresh} < curr_cwnd {curr_cwnd} while in slow start --> invalid state"
        );

        eprintln!(
            "DEBUG: on_packets_acked: pn={}, rtt={:?}, samples={}, in_css={}, window_end={:?}",
            largest_acked,
            rtt_est.latest(),
            self.current.rtt_sample_count,
            self.in_css(),
            self.current.window_end
        );

        self.collect_rtt_sample(rtt_est.latest());

        eprintln!(
            "DEBUG: After collect: samples={}, cur_min={:?}, last_min={:?}",
            self.current.rtt_sample_count,
            self.current.current_round_min_rtt,
            self.current.last_round_min_rtt
        );

        if self.in_css()
            && self.enough_samples()
            && self.current.current_round_min_rtt < self.current.css_baseline_min_rtt
        {
            // this takes us out of CSS again
            qinfo!("exiting CSS after {} rounds", self.current.css_round_count);
            self.current.css_baseline_min_rtt = Duration::MAX;
            self.current.css_round_count = 0;
        }
        if !self.in_css()
            && self.enough_samples()
            && self.current.current_round_min_rtt != Duration::MAX
            && self.current.last_round_min_rtt != Duration::MAX
        {
            let rtt_thresh = max(
                Self::MIN_RTT_THRESH,
                min(
                    self.current.last_round_min_rtt / Self::MIN_RTT_DIVISOR,
                    Self::MAX_RTT_THRESH,
                ),
            );

            eprintln!(
                "DEBUG: CSS check: thresh={:?}, cur={:?}, last={:?}, diff={:?}, need={:?}",
                rtt_thresh,
                self.current.current_round_min_rtt,
                self.current.last_round_min_rtt,
                self.current
                    .current_round_min_rtt
                    .saturating_sub(self.current.last_round_min_rtt),
                self.current.last_round_min_rtt + rtt_thresh
            );

            if self.current.current_round_min_rtt >= self.current.last_round_min_rtt + rtt_thresh {
                self.current.css_baseline_min_rtt = self.current.current_round_min_rtt;
                qinfo!("entered CSS");
                eprintln!("DEBUG: *** ENTERED CSS ***");
            } else {
                eprintln!("DEBUG: Did NOT enter CSS (RTT increase insufficient)");
            }
        } else {
            eprintln!(
                "DEBUG: Skip CSS check: in_css={}, enough={}, cur!=MAX={}, last!=MAX={}",
                self.in_css(),
                self.enough_samples(),
                self.current.current_round_min_rtt != Duration::MAX,
                self.current.last_round_min_rtt != Duration::MAX
            );
        }

        let mut exit_slow_start = false;
        let cwnd_increase = self.calc_cwnd_increase(new_acked, max_datagram_size, self.in_css());

        // check for end of round
        if let Some(window_end) = self.current.window_end {
            if largest_acked >= window_end {
                eprintln!(
                    "DEBUG: Round ended: largest_acked={} >= window_end={}",
                    largest_acked, window_end
                );
                self.current.window_end = None;

                if self.in_css() {
                    exit_slow_start = self.maybe_exit_css();
                }
            }
        }

        SlowStartResult {
            cwnd_increase,
            exit_slow_start,
        }
    }
}
