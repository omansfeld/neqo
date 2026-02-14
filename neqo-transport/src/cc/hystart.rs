// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{
    cmp::{max, min},
    time::Duration,
};

use neqo_common::{qdebug, qinfo};

use crate::{
    cc::{
        classic_cc::{SlowStart, SlowStartResult},
        classic_slow_start::ClassicSlowStart,
    },
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

    const fn maybe_exit_to_ca(&mut self) -> bool {
        self.current.css_round_count += 1;
        self.current.css_round_count >= Self::CSS_ROUNDS
    }

    fn calc_cwnd_increase(&self, new_acked: usize, max_datagram_size: usize, css: bool) -> usize {
        let mut cwnd_increase = min(self.limit.saturating_mul(max_datagram_size), new_acked);

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
        qdebug!("HyStart: maybe_start_new_round -> started new round");
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

        // > An implementation SHOULD use HyStart++ only for the initial slow start (when the
        // > ssthresh is at its initial value of arbitrarily high per [RFC5681]) and fall back to
        // > using standard slow start for the remainder of the connection lifetime. This is
        // > acceptable because subsequent slow starts will use the discovered ssthresh value to
        // > exit slow start and avoid the overshoot problem.
        //
        // <https://datatracker.ietf.org/doc/html/rfc9406#section-4.3-11>
        if ssthresh != usize::MAX {
            qdebug!("HyStart: falling back to classic slow start because ssthresh={ssthresh}!=usize::MAX");
            return ClassicSlowStart::default().on_packets_acked(
                curr_cwnd,
                ssthresh,
                new_acked,
                rtt_est,
                max_datagram_size,
                largest_acked,
            );
        }

        self.collect_rtt_sample(rtt_est.latest());

        qdebug!(
            "HyStart: on_packets_acked -> pn={largest_acked}, rtt={:?}, cur_min={:?}, last_min={:?}, samples={}, in_css={}, css_rounds={}, window_end={:?}",
            rtt_est.latest(),
            self.current.current_round_min_rtt,
            self.current.last_round_min_rtt,
            self.current.rtt_sample_count,
            self.in_css(),
            self.current.css_round_count,
            self.current.window_end
        );

        if self.in_css()
            && self.enough_samples()
            && self.current.current_round_min_rtt < self.current.css_baseline_min_rtt
        {
            qinfo!(
                "HyStart: on_packets_acked -> exiting CSS after {} rounds because cur_min={:?} < baseline_min={:?}",
                self.current.css_round_count,
                self.current.current_round_min_rtt,
                self.current.css_baseline_min_rtt
            );

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

            if self.current.current_round_min_rtt >= self.current.last_round_min_rtt + rtt_thresh {
                self.current.css_baseline_min_rtt = self.current.current_round_min_rtt;
                qinfo!("HyStart: on_packets_acked -> entered CSS because cur_min={:?} >= last_min={:?} + thresh={rtt_thresh:?}",
                       self.current.current_round_min_rtt, self.current.last_round_min_rtt);
            }
        }

        let mut exit_slow_start = false;
        let cwnd_increase = self.calc_cwnd_increase(new_acked, max_datagram_size, self.in_css());

        // check for end of round
        if let Some(window_end) = self.current.window_end {
            if largest_acked >= window_end {
                qdebug!(
                    "HyStart: on_packets_acked -> round ended because largest_acked={largest_acked} >= window_end={window_end}"
                );
                self.current.window_end = None;

                if self.in_css() {
                    exit_slow_start = self.maybe_exit_to_ca();
                    qinfo!("HyStart: on_packets_acked -> exit={exit_slow_start} because css_rounds={} >= {}", self.current.css_round_count, Self::CSS_ROUNDS);
                }
            }
        }

        SlowStartResult {
            cwnd_increase,
            exit_slow_start,
        }
    }
}
