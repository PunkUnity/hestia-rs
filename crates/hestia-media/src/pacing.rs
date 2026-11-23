//! Frame pacing for Android direct video, after Hestia Android's modes.
//!
//! - **Balanced:** each frame gets its own display refresh. A frame is due no
//!   sooner than 80% of the stream's frame interval after the previous one,
//!   on the next vsync, and at most about two frames are scheduled ahead
//!   (Hestia Android holds up to two in its queue). The release carries that
//!   vsync's time, so the system shows the frame at that refresh even when
//!   the decoder thread releases it a little early. If there is no room in
//!   that window, the new output is discarded.
//! - **Min latency:** the newest decoded frame is released at once.
//! - **Max smoothness:** the newest decoded frame is released with a zero
//!   timestamp, which Hestia Android uses so the display shows every frame
//!   it is given.
//!
//! The vsync times come from the Choreographer on a helper thread
//! ([`VsyncClock`]); [`Schedule`] is the timing rule and has no Android
//! dependency, so it is tested on the host.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pacing {
    Balanced,
    #[default]
    MinLatency,
    MaxSmoothness,
}

impl Pacing {
    pub fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::MinLatency => "Min latency",
            Self::MaxSmoothness => "Max smoothness",
        }
    }
}

static MODE: AtomicU8 = AtomicU8::new(Pacing::MinLatency as u8);

/// The pacing the next decoder start uses.
pub fn set_mode(mode: Pacing) {
    MODE.store(mode as u8, Ordering::Release);
}

pub fn mode() -> Pacing {
    match MODE.load(Ordering::Acquire) {
        0 => Pacing::Balanced,
        2 => Pacing::MaxSmoothness,
        _ => Pacing::MinLatency,
    }
}

/// The most frames scheduled ahead of now, as Hestia Android's queue limit.
const MAX_AHEAD_FRAMES: i64 = 2;

/// Balanced timing: the vsync each released frame is shown at.
#[derive(Debug, Default)]
pub struct Schedule {
    /// Target refresh and release timestamp of the last scheduled frame.
    last: Option<(i64, i64)>,
}

impl Schedule {
    /// The vsync (CLOCK_MONOTONIC ns) for a frame released at `now`, given a
    /// recent vsync time and the refresh period, for a stream with frames
    /// `frame_ns` apart. None means the next slot is too far ahead.
    pub fn next(&mut self, now: i64, vsync: i64, period: i64, frame_ns: i64) -> Option<i64> {
        let period = period.max(1);
        let frame_ns = frame_ns.max(1);
        // The first refresh at or after `t`.
        let slot = |t: i64| {
            vsync
                + (t - vsync).div_euclid(period) * period
                + if (t - vsync).rem_euclid(period) == 0 { 0 } else { period }
        };
        let earliest = self.last.map_or(now, |(target, released_at)| {
            // A refresh-rate change can also shift the half-refresh offset.
            now.max(target + (frame_ns * 4 / 5).max(1))
                .max(released_at + period / 2 + 1)
        });
        let target = slot(earliest);
        // The codec already owns the scheduled buffers. Keep their order
        // when the next frame would exceed the latency window.
        if target - now > MAX_AHEAD_FRAMES * frame_ns + period {
            return None;
        }
        self.last = Some((target, Self::release_time(target, period)));
        Some(target)
    }

    /// Forget the previous frame (codec flushed or restarted).
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// The timestamp to release with for a frame due at `target`: half a
    /// refresh early, so it lands on that refresh whatever the offset
    /// between the Choreographer's time and the display's.
    pub fn release_time(target: i64, period: i64) -> i64 {
        target - period / 2
    }
}

#[cfg(target_os = "android")]
pub use clock::VsyncClock;

#[cfg(target_os = "android")]
mod clock {
    //! The display's vsync times, from the Choreographer on a helper thread
    //! with its own looper (as the Slint backend's vsync driver). The thread
    //! runs while some decoder holds a [`VsyncClock`].

    use std::collections::VecDeque;
    use std::ffi::{c_long, c_void};
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    use log::{info, warn};
    use ndk::looper::ThreadLooper;

    #[repr(C)]
    struct AChoreographer {
        _unused: [u8; 0],
    }
    type FrameCallback = unsafe extern "C" fn(frame_time_nanos: c_long, data: *mut c_void);

    #[link(name = "android")]
    unsafe extern "C" {
        fn AChoreographer_getInstance() -> *mut AChoreographer;
        fn AChoreographer_postFrameCallback(
            choreographer: *mut AChoreographer,
            callback: FrameCallback,
            data: *mut c_void,
        );
    }

    static LAST_VSYNC: AtomicI64 = AtomicI64::new(0);
    static PERIOD: AtomicI64 = AtomicI64::new(0);
    static USERS: AtomicUsize = AtomicUsize::new(0);
    static WAKE: Condvar = Condvar::new();
    static PARK: Mutex<()> = Mutex::new(());
    static STARTED: OnceLock<bool> = OnceLock::new();

    /// Deltas between consecutive callbacks; the refresh period is the
    /// smallest, as a missed callback only makes a delta longer.
    const PERIOD_SAMPLES: usize = 32;
    const MIN_PERIOD_NS: i64 = 4_000_000;
    const MAX_PERIOD_NS: i64 = 50_000_000;

    /// Keeps the vsync thread running while held.
    pub struct VsyncClock(());

    impl VsyncClock {
        pub fn start() -> Self {
            let started = *STARTED.get_or_init(|| {
                std::thread::Builder::new()
                    .name("video vsync".into())
                    .spawn(run)
                    .map_err(|e| warn!("frame pacing: no vsync thread: {e}"))
                    .is_ok()
            });
            if started {
                USERS.fetch_add(1, Ordering::AcqRel);
                WAKE.notify_all();
            }
            Self(())
        }

        /// A recent vsync time and the refresh period (CLOCK_MONOTONIC ns),
        /// once the thread has measured them.
        pub fn latest(&self) -> Option<(i64, i64)> {
            let period = PERIOD.load(Ordering::Acquire);
            let vsync = LAST_VSYNC.load(Ordering::Acquire);
            (period > 0 && vsync > 0).then_some((vsync, period))
        }
    }

    impl Drop for VsyncClock {
        fn drop(&mut self) {
            if STARTED.get() == Some(&true) {
                USERS.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    struct Samples {
        last: Option<i64>,
        deltas: VecDeque<i64>,
    }

    unsafe extern "C" fn on_frame(frame_time_nanos: c_long, data: *mut c_void) {
        // SAFETY: `data` is the `Samples` owned by `run`, which waits in
        // `poll_once` on this same thread while the callback runs.
        let samples = unsafe { &mut *data.cast::<Samples>() };
        let now = frame_time_nanos as i64;
        if let Some(last) = samples.last
            && (MIN_PERIOD_NS..=MAX_PERIOD_NS * 4).contains(&(now - last))
        {
            if samples.deltas.len() == PERIOD_SAMPLES {
                samples.deltas.pop_front();
            }
            samples.deltas.push_back(now - last);
            if let Some(&period) = samples.deltas.iter().min()
                && period <= MAX_PERIOD_NS
            {
                PERIOD.store(period, Ordering::Release);
            }
        }
        samples.last = Some(now);
        LAST_VSYNC.store(now, Ordering::Release);
    }

    fn run() {
        let looper = ThreadLooper::prepare();
        // SAFETY: the calling thread has a looper (prepared above).
        let choreographer = unsafe { AChoreographer_getInstance() };
        if choreographer.is_null() {
            warn!("frame pacing: no Choreographer; frames go out unpaced");
            return;
        }
        info!("frame pacing: vsync thread running");
        let mut samples = Samples {
            last: None,
            deltas: VecDeque::with_capacity(PERIOD_SAMPLES),
        };
        loop {
            if USERS.load(Ordering::Acquire) == 0 {
                // Idle between streams; a pause breaks the delta chain.
                samples.last = None;
                let guard = PARK.lock().unwrap_or_else(|p| p.into_inner());
                let _ = WAKE.wait_timeout(guard, Duration::from_secs(1));
                continue;
            }
            // SAFETY: `choreographer` belongs to this thread; `samples`
            // outlives the callback, which runs inside `poll_once` below.
            unsafe {
                AChoreographer_postFrameCallback(choreographer, on_frame, (&raw mut samples).cast());
            }
            let _ = looper.poll_once_timeout(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    // 120 Hz display, 60 fps stream.
    const P120: i64 = 8_333_333;
    const F60: i64 = 16_666_667;

    #[test]
    fn steady_60_fps_on_120_hz_uses_every_other_refresh() {
        let mut s = Schedule::default();
        let vsync = 1_000 * MS;
        let mut targets = Vec::new();
        for i in 0..6 {
            // Frames decoded 3 ms after their refresh.
            let now = vsync + i * F60 + 3 * MS;
            targets.push(s.next(now, vsync, P120, F60).unwrap());
        }
        let gaps: Vec<i64> = targets.windows(2).map(|w| (w[1] - w[0] + P120 / 2) / P120).collect();
        assert_eq!(gaps, vec![2, 2, 2, 2, 2], "one frame every two refreshes: {targets:?}");
        for (i, t) in targets.iter().enumerate() {
            assert_eq!((t - vsync).rem_euclid(P120), 0, "frame {i} lands on a refresh");
        }
    }

    #[test]
    fn a_burst_is_spread_over_refreshes_and_capped() {
        let vsync = 1_000 * MS;
        let now = vsync + MS;
        for (period, spacing) in [(P120, 2 * P120), (F60, F60)] {
            let mut s = Schedule::default();
            for i in 0..3 {
                let target = s.next(now, vsync, period, F60).unwrap();
                assert_eq!(target, vsync + period + i * spacing);
                assert!(target - now <= MAX_AHEAD_FRAMES * F60 + period);
            }
            assert_eq!(s.next(now, vsync, period, F60), None);
            assert_eq!(s.next(now, vsync, period, F60), None);
            assert_eq!(
                s.next(now + spacing, vsync, period, F60),
                Some(vsync + period + 3 * spacing)
            );
        }
    }

    #[test]
    fn choosing_a_different_refresh_period_preserves_release_order() {
        let mut s = Schedule::default();
        let now = 1_000 * MS;
        let last = s.next(now, now, P120, P120).unwrap();
        let slower_period = P120 * 4;
        let first_slower_refresh = now + P120;
        let next = s.next(now, first_slower_refresh, slower_period, P120).unwrap();
        assert!(Schedule::release_time(next, slower_period) > Schedule::release_time(last, P120));
    }

    #[test]
    fn same_rate_stream_and_display() {
        let p60 = F60;
        let mut s = Schedule::default();
        let vsync = 500 * MS;
        let a = s.next(vsync + 2 * MS, vsync, p60, F60);
        let b = s.next(vsync + p60 + 2 * MS, vsync, p60, F60);
        assert_eq!((a, b), (Some(vsync + p60), Some(vsync + 2 * p60)));
        // A frame that arrives a little early still gets the next refresh.
        let c = s.next(vsync + 2 * p60 - 5 * MS, vsync, p60, F60);
        assert_eq!(c, Some(vsync + 3 * p60));
    }

    #[test]
    fn the_vsync_sample_can_be_older_or_newer_than_now() {
        let mut s = Schedule::default();
        let vsync = 1_000 * MS;
        let t = s.next(vsync - 20 * MS, vsync, P120, F60).unwrap();
        assert!(t >= vsync - 20 * MS && (t - vsync).rem_euclid(P120) == 0);
        s.reset();
        let t = s.next(vsync + 100 * MS, vsync, P120, F60).unwrap();
        assert!(t >= vsync + 100 * MS && t - (vsync + 100 * MS) < P120);
    }

    #[test]
    fn release_time_is_half_a_refresh_early() {
        assert_eq!(Schedule::release_time(100 * MS, 8 * MS), 96 * MS);
    }
}
