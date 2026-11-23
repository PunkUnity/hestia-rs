//! Per-stream accounting for the Android direct MediaCodec output.
//!
//! A missing callback is not proof that a frame was dropped. Keep outstanding
//! releases separate from explicit discards and time decoder-output gaps even
//! when there are no render timestamps.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::decode::LatencyHistogram;

const MAX_PENDING: usize = 1024;
const CALLBACK_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub struct DirectVideoStats(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    codec: Option<usize>,
    callbacks: bool,
    ever_active: bool,
    ever_measured: bool,
    period_ns: u64,
    pending: VecDeque<(i64, Instant)>,
    released: u64,
    tracked: u64,
    discarded: u64,
    hidden: u64,
    failed: u64,
    matched: u64,
    aged_out: u64,
    retired: u64,
    unmatched: u64,
    last_render_ns: Option<i64>,
    intervals: LatencyHistogram,
    interval_count: u64,
    uneven: u64,
    worst: Duration,
    callback_delay: LatencyHistogram,
    last_output: Option<Instant>,
    worst_output_gap: Duration,
}

impl State {
    fn observe_output_gap(&mut self, now: Instant) {
        if let Some(previous) = self.last_output {
            self.worst_output_gap = self.worst_output_gap.max(now.saturating_duration_since(previous));
        }
    }

    fn reset_render(&mut self, now: Instant) {
        self.last_render_ns = None;
        self.expire(now);
        self.retired += self.pending.len() as u64;
        self.pending.clear();
    }

    fn finish_output(&mut self, now: Instant) {
        self.observe_output_gap(now);
        self.last_output = None;
        self.reset_render(now);
    }

    fn expire(&mut self, now: Instant) {
        while self
            .pending
            .front()
            .is_some_and(|&(_, queued)| now.saturating_duration_since(queued) > CALLBACK_GRACE)
        {
            self.pending.pop_front();
            self.aged_out += 1;
        }
    }
}

impl DirectVideoStats {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn begin_codec(&self, id: usize, fps: u32, now: Instant) {
        let mut s = self.lock();
        s.finish_output(now);
        s.codec = Some(id);
        s.callbacks = false;
        s.ever_active = true;
        s.period_ns = 1_000_000_000 / u64::from(fps.max(1));
    }

    pub(crate) fn enable_callbacks(&self, id: usize) {
        let mut s = self.lock();
        if s.codec == Some(id) {
            s.callbacks = true;
            s.ever_measured = true;
        }
    }

    pub(crate) fn surface_changed(&self, id: usize, now: Instant) {
        let mut s = self.lock();
        if s.codec == Some(id) {
            // The decoder may have noticed the change after the surface
            // disappeared. Do not treat that period as visible output time.
            s.last_output = None;
            s.reset_render(now);
        }
    }

    pub(crate) fn codec_flushed(&self, id: usize, now: Instant) {
        let mut s = self.lock();
        if s.codec == Some(id) {
            // PTS associations are reset, but any continuing decoder-output
            // gap on this surface must still be included in the report.
            s.reset_render(now);
        }
    }

    pub(crate) fn end_codec(&self, id: usize, now: Instant) {
        let mut s = self.lock();
        if s.codec == Some(id) {
            s.finish_output(now);
            s.codec = None;
            s.callbacks = false;
        }
    }

    pub(crate) fn output(&self, id: usize, now: Instant) {
        let mut s = self.lock();
        if s.codec == Some(id) {
            s.observe_output_gap(now);
            s.last_output = Some(now);
        }
    }

    /// Called before release: a callback can race the successful return.
    pub(crate) fn expect_callback(&self, id: usize, pts: i64, now: Instant) {
        let mut s = self.lock();
        if s.codec != Some(id) || !s.callbacks {
            return;
        }
        s.expire(now);
        if s.pending.len() == MAX_PENDING {
            s.pending.pop_front();
            s.aged_out += 1;
        }
        s.pending.push_back((pts, now));
    }

    pub(crate) fn release_result(&self, id: usize, pts: i64, visible: bool, ok: bool) {
        let mut s = self.lock();
        if s.codec != Some(id) {
            return;
        }
        match (visible, ok) {
            (true, true) => {
                s.released += 1;
                if s.callbacks {
                    s.tracked += 1;
                }
            }
            (false, true) => s.hidden += 1,
            (_, false) => {
                s.failed += 1;
                if visible {
                    s.pending.retain(|&(pending_pts, _)| pending_pts != pts);
                }
            }
        }
    }

    pub(crate) fn discard_older(&self, id: usize, visible: bool) {
        let mut s = self.lock();
        if s.codec == Some(id) {
            if visible {
                s.discarded += 1;
            } else {
                s.hidden += 1;
            }
        }
    }

    pub(crate) fn callback(&self, id: usize, pts: i64, render_ns: i64, arrival: Instant) {
        let mut s = self.lock();
        if s.codec != Some(id) || !s.callbacks {
            return;
        }
        let Some(index) = s.pending.iter().position(|&(pending_pts, _)| pending_pts == pts) else {
            s.unmatched += 1;
            return;
        };
        let (_, release) = s.pending.remove(index).expect("matched pending release");
        s.matched += 1;
        // The codec timestamp can have a different origin on some devices.
        // Use the app clock for release-to-callback and the codec clock only
        // for differences between consecutive render timestamps.
        s.callback_delay.record(arrival.saturating_duration_since(release));
        if render_ns <= 0 {
            return;
        }
        if let Some(previous) = s.last_render_ns {
            // An older timestamp can be delivered after a newer one. Keep
            // the latest timestamp as the predecessor for interval sampling.
            if render_ns <= previous {
                return;
            }
            let ns = (render_ns - previous) as u64;
            let interval = Duration::from_nanos(ns);
            s.intervals.record(interval);
            s.interval_count += 1;
            s.uneven += u64::from(u128::from(ns.abs_diff(s.period_ns)) * 10 > u128::from(s.period_ns) * 3);
            s.worst = s.worst.max(interval);
        }
        s.last_render_ns = Some(render_ns);
    }

    /// Explicitly replaced pictures while the surface was visible. Callback
    /// accounting is deliberately excluded from the live drop estimate.
    pub fn superseded(&self) -> u64 {
        self.lock().discarded
    }

    pub fn report(&self, now: Instant) -> Vec<(&'static str, String)> {
        let mut s = self.lock();
        if !s.ever_active {
            return Vec::new();
        }
        s.expire(now);
        s.observe_output_gap(now);
        let mut rows = vec![
            ("visible releases", s.released.to_string()),
            ("older output discarded", s.discarded.to_string()),
            ("output discarded while surface was hidden", s.hidden.to_string()),
            ("release failures", s.failed.to_string()),
            (
                "longest visible decoder-output gap, including tail",
                if s.worst_output_gap.is_zero() {
                    "no gap samples".to_owned()
                } else {
                    format!("{:.2} ms", s.worst_output_gap.as_secs_f64() * 1000.0)
                },
            ),
        ];
        if s.ever_measured {
            rows.extend([
                ("visible releases with callback tracking", s.tracked.to_string()),
                ("matched render callbacks", s.matched.to_string()),
                ("awaiting callback", s.pending.len().to_string()),
                ("callback tracking expired or full", s.aged_out.to_string()),
                ("unconfirmed at surface, codec or stream end", s.retired.to_string()),
                ("callback without a tracked release", s.unmatched.to_string()),
            ]);
            if let Some(intervals) = s.intervals.summary() {
                rows.push((
                    "codec render timestamp intervals",
                    format!(
                        "{intervals}; worst {:.2} ms; {:.1}% uneven (outside ±30%)",
                        s.worst.as_secs_f64() * 1000.0,
                        s.uneven as f64 * 100.0 / s.interval_count as f64
                    ),
                ));
            }
            if let Some(delay) = s.callback_delay.summary() {
                rows.push(("visible buffer release to callback arrival", delay));
            }
        } else {
            rows.push(("render callbacks", "unavailable".to_owned()));
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_callbacks_and_failures_do_not_become_display_drops() {
        let stats = DirectVideoStats::default();
        let now = Instant::now();
        stats.begin_codec(1, 60, now);
        stats.enable_callbacks(1);
        for pts in 0..3 {
            stats.expect_callback(1, pts, now);
            stats.release_result(1, pts, true, true);
        }
        stats.expect_callback(1, 4, now);
        stats.release_result(1, 4, true, false);
        stats.release_result(1, 5, false, true);
        assert_eq!(stats.superseded(), 0);
        assert!(
            stats
                .report(now + Duration::from_secs(1))
                .contains(&("awaiting callback", "3".into()))
        );
        for pts in 0..3 {
            stats.callback(
                1,
                pts,
                1_000_000_000 + pts * 16_666_667,
                now + Duration::from_millis(20),
            );
        }
        let s = stats.lock();
        assert_eq!((s.released, s.matched, s.failed, s.hidden), (3, 3, 1, 1));
        assert_eq!(s.interval_count, 2);
        assert_eq!(
            s.callback_delay.summary().as_deref(),
            Some("p50 20.00 / p95 20.00 / p99 20.00 ms (3 samples)")
        );
        assert!(s.pending.is_empty());
        drop(s);
        stats.discard_older(1, true);
        stats.discard_older(1, false);
        assert_eq!(stats.superseded(), 1);
        assert_eq!(stats.lock().hidden, 2);
    }

    #[test]
    fn output_stall_and_tail_are_measured_without_callbacks() {
        let stats = DirectVideoStats::default();
        let now = Instant::now();
        stats.begin_codec(1, 60, now);
        stats.output(1, now);
        stats.codec_flushed(1, now + Duration::from_millis(100));
        stats.output(1, now + Duration::from_secs(2));
        assert_eq!(stats.lock().worst_output_gap, Duration::from_secs(2));
        stats.end_codec(1, now + Duration::from_secs(5));
        let s = stats.lock();
        assert_eq!(s.worst_output_gap, Duration::from_secs(3));
        assert_eq!(s.interval_count, 0);
    }

    #[test]
    fn surface_and_codec_boundaries_keep_callbacks_separate() {
        let stats = DirectVideoStats::default();
        let now = Instant::now();
        stats.begin_codec(1, 60, now);
        stats.enable_callbacks(1);
        stats.expect_callback(1, 7, now);
        stats.output(1, now);
        stats.surface_changed(1, now + Duration::from_secs(1));
        stats.callback(1, 7, 100, now);
        stats.begin_codec(2, 60, now);
        stats.enable_callbacks(2);
        stats.expect_callback(2, 7, now);
        stats.callback(1, 7, 200, now);
        stats.callback(2, 7, 300, now);
        let s = stats.lock();
        assert_eq!((s.retired, s.unmatched, s.matched, s.interval_count), (1, 1, 1, 0));
        assert!(s.worst_output_gap.is_zero());
    }

    #[test]
    fn interval_histogram_keeps_late_samples_and_tracking_is_bounded() {
        let stats = DirectVideoStats::default();
        let now = Instant::now();
        stats.begin_codec(1, 120, now);
        stats.enable_callbacks(1);
        for pts in 0..=65_536 {
            stats.expect_callback(1, pts, now);
            stats.callback(1, pts, 1_000_000_000 + pts * 8_333_333, now);
        }
        stats.expect_callback(1, 65_537, now);
        stats.callback(1, 65_537, 3_000_000_000 + 65_536 * 8_333_333, now);
        for pts in 100_000..100_000 + MAX_PENDING as i64 + 1 {
            stats.expect_callback(1, pts, now);
        }
        assert!(
            stats
                .report(now + CALLBACK_GRACE + Duration::from_millis(1))
                .contains(&("callback tracking expired or full", (MAX_PENDING + 1).to_string()))
        );
        let s = stats.lock();
        assert_eq!(s.interval_count, 65_537);
        assert_eq!(s.worst, Duration::from_secs(2));
        assert!(s.pending.is_empty());
    }
}
