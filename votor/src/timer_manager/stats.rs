use {
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimerManagerStats {
    /// The maximum heap size of the timers since the last report
    max_heap_size: u64,
    /// The number of times `set_timeout` was called.
    set_timeout_count: u64,
    /// The number of times `set_timeout` was called, there was no
    /// existing timer, so this operation succeeded.
    set_timeout_succeed_count: u64,
    /// The number of timer events sent into the event handler queue.
    event_send_count: u64,
    /// The number of timer event sends that found the event handler queue full.
    event_send_blocked_count: u64,
    /// Total time spent sending timer events into the event handler queue.
    event_send_time_us: u64,
    /// Maximum time spent on a single timer event send.
    event_send_max_time_us: u64,
    /// The last time the stats were reported
    last_report: Instant,
}

impl TimerManagerStats {
    pub fn new() -> Self {
        Self {
            max_heap_size: 0,
            set_timeout_count: 0,
            set_timeout_succeed_count: 0,
            event_send_count: 0,
            event_send_blocked_count: 0,
            event_send_time_us: 0,
            event_send_max_time_us: 0,
            last_report: Instant::now(),
        }
    }

    #[cfg(test)]
    pub fn max_heap_size(&self) -> u64 {
        self.max_heap_size
    }

    #[cfg(test)]
    pub fn set_timeout_count(&self) -> u64 {
        self.set_timeout_count
    }

    #[cfg(test)]
    pub fn set_timeout_succeed_count(&self) -> u64 {
        self.set_timeout_succeed_count
    }

    pub fn incr_timeout_count_with_heap_size(&mut self, size: usize, new_timer_inserted: bool) {
        self.set_timeout_count = self.set_timeout_count.saturating_add(1);
        self.max_heap_size = self.max_heap_size.max(size as u64);
        if new_timer_inserted {
            self.set_timeout_succeed_count = self.set_timeout_succeed_count.saturating_add(1);
        }
        self.maybe_report();
    }

    pub fn record_event_send(&mut self, duration: Duration, blocked: bool) {
        let time_us = duration.as_micros().min(u64::MAX as u128) as u64;
        self.event_send_count = self.event_send_count.saturating_add(1);
        if blocked {
            self.event_send_blocked_count = self.event_send_blocked_count.saturating_add(1);
        }
        self.event_send_time_us = self.event_send_time_us.saturating_add(time_us);
        self.event_send_max_time_us = self.event_send_max_time_us.max(time_us);
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        if self.last_report.elapsed() < STATS_REPORT_INTERVAL {
            return;
        }
        datapoint_info!(
            "votor_timer_manager",
            ("max_heap_size", self.max_heap_size as i64, i64),
            ("set_timeout_count", self.set_timeout_count as i64, i64),
            (
                "set_timeout_succeed_count",
                self.set_timeout_succeed_count as i64,
                i64
            ),
            ("event_send_count", self.event_send_count as i64, i64),
            (
                "event_send_blocked_count",
                self.event_send_blocked_count as i64,
                i64
            ),
            ("event_send_time_us", self.event_send_time_us as i64, i64),
            (
                "event_send_max_time_us",
                self.event_send_max_time_us as i64,
                i64
            ),
        );
        *self = TimerManagerStats::new();
    }
}
