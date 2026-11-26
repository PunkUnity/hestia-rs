//! Balanced Direct video releases at most once per display refresh. When
//! that refresh already has a frame, wait to drain output until the next
//! slot so the newest available picture can take it.

#[derive(Clone, Copy)]
pub(crate) struct DisplaySlot {
    refresh_ns: i64,
    period_ns: i64,
    pub(crate) timestamp_ns: i64,
}

#[derive(Default)]
pub(crate) struct ReleaseSchedule(Option<DisplaySlot>);

impl ReleaseSchedule {
    pub(crate) fn next(&self, now: i64, vsync: i64, period: i64) -> Option<DisplaySlot> {
        let earliest = now + period / 2;
        let offset = earliest - vsync;
        let refresh_ns = earliest + (period - offset.rem_euclid(period)).rem_euclid(period);
        let timestamp_ns = refresh_ns - period / 2;
        if self.0.is_some_and(|last| {
            refresh_ns - last.refresh_ns < last.period_ns.min(period) / 2 || timestamp_ns <= last.timestamp_ns
        }) {
            return None;
        }
        Some(DisplaySlot {
            refresh_ns,
            period_ns: period,
            timestamp_ns,
        })
    }

    pub(crate) fn released(&mut self, slot: DisplaySlot) {
        self.0 = Some(slot);
    }
}

#[cfg(target_os = "android")]
pub(crate) use clock::VsyncClock;

#[cfg(target_os = "android")]
mod clock {
    use std::collections::VecDeque;
    use std::ffi::{c_long, c_void};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    use ndk::looper::ThreadLooper;

    #[repr(C)]
    struct AChoreographer {
        _unused: [u8; 0],
    }

    #[link(name = "android")]
    unsafe extern "C" {
        fn AChoreographer_getInstance() -> *mut AChoreographer;
        fn AChoreographer_postFrameCallback(
            choreographer: *mut AChoreographer,
            callback: unsafe extern "C" fn(c_long, *mut c_void),
            data: *mut c_void,
        );
    }

    struct ClockState {
        users: usize,
        sample: Option<(i64, i64)>,
    }

    static STATE: Mutex<ClockState> = Mutex::new(ClockState { users: 0, sample: None });
    static WAKE: Condvar = Condvar::new();
    static STARTED: OnceLock<bool> = OnceLock::new();

    pub(crate) struct VsyncClock(bool);

    impl VsyncClock {
        pub(crate) fn start() -> Self {
            let started = *STARTED.get_or_init(|| {
                std::thread::Builder::new()
                    .name("video vsync".into())
                    .spawn(run)
                    .map_err(|e| log::warn!("frame pacing: no vsync thread: {e}"))
                    .is_ok()
            });
            if started {
                STATE.lock().unwrap_or_else(|p| p.into_inner()).users += 1;
                WAKE.notify_one();
            }
            Self(started)
        }

        /// CLOCK_MONOTONIC now, the last vsync timestamp and its refresh period.
        /// Wait out warm-up or fall back to immediate releases when vsync stops.
        pub(crate) fn latest(&self) -> Option<(i64, i64, i64)> {
            let (vsync, period) = STATE.lock().unwrap_or_else(|p| p.into_inner()).sample?;
            let now = monotonic_ns();
            (now - vsync <= 3 * period).then_some((now, vsync, period))
        }
    }

    impl Drop for VsyncClock {
        fn drop(&mut self) {
            if self.0 {
                let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
                state.users -= 1;
                if state.users == 0 {
                    state.sample = None;
                }
            }
        }
    }

    #[allow(clippy::unnecessary_cast)] // timespec fields vary by Android ABI.
    fn monotonic_ns() -> i64 {
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `now` is valid writable memory for a timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        now.tv_sec as i64 * 1_000_000_000 + now.tv_nsec as i64
    }

    #[derive(Default)]
    struct Samples {
        pending: bool,
        last: Option<i64>,
        periods: VecDeque<i64>,
    }

    const SAMPLE_COUNT: usize = 12;

    unsafe extern "C" fn on_frame(timestamp: c_long, data: *mut c_void) {
        // SAFETY: `data` points to a Samples on the same running looper thread.
        let samples = unsafe { &mut *data.cast::<Samples>() };
        samples.pending = false;
        #[cfg(target_pointer_width = "32")]
        let timestamp = {
            // The API 26 callback uses a signed C long. Recover its full
            // monotonic timestamp from the closest value to callback time.
            let now = monotonic_ns();
            now - i64::from((now as i32).wrapping_sub(timestamp))
        };
        if let Some(last) = samples.last.replace(timestamp) {
            let delta = timestamp - last;
            if (4_000_000..=50_000_000).contains(&delta) {
                if samples.periods.len() == SAMPLE_COUNT {
                    samples.periods.pop_front();
                }
                samples.periods.push_back(delta);
            } else {
                samples.periods.clear();
            }
        }
        // A missed callback is a multiple of the refresh interval. Use the
        // shortest recent interval and let older modes age out quickly.
        let sample = samples.periods.iter().min().map(|&period| (timestamp, period));
        let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
        if state.users > 0 {
            state.sample = sample;
        }
    }

    fn run() {
        let looper = ThreadLooper::prepare();
        // SAFETY: The thread has an Android looper and owns this Choreographer.
        let choreographer = unsafe { AChoreographer_getInstance() };
        if choreographer.is_null() {
            log::warn!("frame pacing: no Choreographer; frames go out unpaced");
            return;
        }
        let mut samples = Samples::default();
        loop {
            let state = STATE.lock().unwrap_or_else(|p| p.into_inner());
            if state.users == 0 {
                samples.last = None;
                samples.periods.clear();
                drop(
                    WAKE.wait_while(state, |s| s.users == 0)
                        .unwrap_or_else(|p| p.into_inner()),
                );
            } else {
                drop(state);
            }
            if !samples.pending {
                samples.pending = true;
                // SAFETY: the callback runs on this looper, and the Samples
                // is alive on this thread for the entire loop.
                unsafe { AChoreographer_postFrameCallback(choreographer, on_frame, (&raw mut samples).cast()) };
            }
            let _ = looper.poll_once_timeout(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: i64 = 8_333_333;
    const START: i64 = 1_000_000_000;

    #[test]
    fn steady_input_keeps_its_cadence_on_a_120_hz_screen() {
        for spacing in [PERIOD, 2 * PERIOD, 4 * PERIOD] {
            let mut schedule = ReleaseSchedule::default();
            for frame in 0..12 {
                let now = START + frame * spacing + 1_000_000;
                let slot = schedule.next(now, START, PERIOD).unwrap();
                assert_eq!(slot.refresh_ns, START + frame * spacing + PERIOD);
                assert!((now..now + PERIOD).contains(&slot.timestamp_ns));
                schedule.released(slot);
            }
        }
    }

    #[test]
    fn burst_leaves_the_next_slot_free_for_newest_output() {
        let mut schedule = ReleaseSchedule::default();
        let first = schedule.next(START, START, PERIOD).unwrap();
        schedule.released(first);
        for tenth in 1..=4 {
            assert!(schedule.next(START + tenth * PERIOD / 10, START, PERIOD).is_none());
        }
        assert!(schedule.next(START + 10_000, START + 10_000, PERIOD).is_none());
        let next = schedule.next(START + PERIOD, START, PERIOD).unwrap();
        assert_eq!(next.refresh_ns, first.refresh_ns + PERIOD);
    }

    #[test]
    fn slower_refresh_keeps_release_timestamps_in_order() {
        let mut schedule = ReleaseSchedule::default();
        let first = schedule.next(START, START, PERIOD).unwrap();
        schedule.released(first);
        let slow = 4 * PERIOD;
        let next = schedule.next(START + slow, START, slow).unwrap();
        assert!(next.timestamp_ns > first.timestamp_ns);
    }
}
