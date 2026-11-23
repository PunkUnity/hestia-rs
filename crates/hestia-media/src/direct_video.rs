//! Android direct video: MediaCodec renders straight to a surface behind
//! the window (a SurfaceView the Java helper adds), and the system
//! composes it, often as a hardware overlay. The picture never goes through
//! the UI's frame, so a slow UI frame can't hold the decoder back.
//!
//! The frontend says whether the stream wants it ([`set_wanted`]) and hands
//! over the surface as it comes and goes ([`set_surface`]); the decoder
//! picks it up when it starts and follows later changes. When the surface
//! is missing or fails, the decoder uses its other outputs.
//!
//! [`PRESENTED`] records when each frame actually reached the display
//! (`AMediaCodec_setOnFrameRenderedCallback`, Android 13+), which is what
//! the stream report shows as "Direct video".

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{info, warn};
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;

static WANTED: AtomicBool = AtomicBool::new(false);

struct Slot {
    window: Option<NativeWindow>,
    generation: u64,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    window: None,
    generation: 0,
});

fn slot() -> std::sync::MutexGuard<'static, Slot> {
    SLOT.lock().unwrap_or_else(|p| p.into_inner())
}

/// The stream asks for direct video (the setting is on and the helper is
/// adding the surface).
pub fn set_wanted(on: bool) {
    WANTED.store(on, Ordering::Release);
}

pub fn wanted() -> bool {
    WANTED.load(Ordering::Acquire)
}

/// The surface appeared (`Some`) or was destroyed (`None`).
pub fn set_surface(window: Option<NativeWindow>) {
    let mut slot = slot();
    info!(
        "direct video surface {}",
        if window.is_some() { "ready" } else { "gone" }
    );
    slot.window = window;
    slot.generation += 1;
}

/// `ANativeWindow_fromSurface` for the helper's surface (null: gone).
///
/// # Safety
/// `env` is the calling thread's `JNIEnv *` and `surface` a local reference
/// to an `android.view.Surface` or null.
pub unsafe fn set_surface_from_java(env: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) {
    let window = if surface.is_null() {
        None
    } else {
        // SAFETY: per this function's contract.
        unsafe { NativeWindow::from_surface(env.cast(), surface.cast()) }
    };
    set_surface(window);
}

/// The surface now, and its generation (bumped on every change).
pub(crate) fn current() -> (u64, Option<NativeWindow>) {
    let slot = slot();
    (slot.generation, slot.window.clone())
}

/// Waits a little for the surface: the helper adds it on the UI thread
/// while the stream starts, so it can arrive just after the decoder.
pub(crate) fn wait_for_surface(timeout: Duration) -> Option<(u64, NativeWindow)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let (generation, Some(window)) = current() {
            return Some((generation, window));
        }
        if !wanted() || Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// When frames reached the display, from the codec's frame-rendered
/// callback.
pub struct PresentedStats {
    frames: AtomicU64,
    last_ns: AtomicI64,
    period_us: AtomicU64,
    /// Every interval between two presented frames, microseconds (capped,
    /// about 18 minutes at 60 fps).
    intervals: Mutex<Vec<u32>>,
    /// Whether the callback could be installed for this stream.
    measuring: AtomicBool,
}

const MAX_INTERVALS: usize = 1 << 16;

pub static PRESENTED: PresentedStats = PresentedStats {
    frames: AtomicU64::new(0),
    last_ns: AtomicI64::new(0),
    period_us: AtomicU64::new(0),
    intervals: Mutex::new(Vec::new()),
    measuring: AtomicBool::new(false),
};

/// A summary of [`PresentedStats`] for the report and the overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresentedSummary {
    pub frames: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub worst_ms: f64,
    /// Intervals more than 30% away from the stream's frame period.
    pub uneven_pct: f64,
}

impl PresentedStats {
    /// A new stream at `fps`.
    pub fn reset(&self, fps: u32) {
        self.frames.store(0, Ordering::Relaxed);
        self.last_ns.store(0, Ordering::Relaxed);
        self.period_us
            .store(1_000_000 / u64::from(fps.max(1)), Ordering::Relaxed);
        self.intervals.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.measuring.store(false, Ordering::Relaxed);
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn measuring(&self) -> bool {
        self.measuring.load(Ordering::Relaxed)
    }

    fn record(&self, system_ns: i64) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let last = self.last_ns.swap(system_ns, Ordering::Relaxed);
        if last > 0 && system_ns > last {
            let us = ((system_ns - last) / 1_000).min(i64::from(u32::MAX)) as u32;
            let mut intervals = self.intervals.lock().unwrap_or_else(|p| p.into_inner());
            if intervals.len() < MAX_INTERVALS {
                intervals.push(us);
            }
        }
    }

    pub fn summary(&self) -> Option<PresentedSummary> {
        let mut intervals = self.intervals.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if intervals.is_empty() {
            return None;
        }
        intervals.sort_unstable();
        let at = |q: f64| intervals[((intervals.len() - 1) as f64 * q).round() as usize] as f64 / 1_000.0;
        let period = self.period_us.load(Ordering::Relaxed).max(1) as f64;
        let uneven = intervals
            .iter()
            .filter(|&&us| (us as f64 - period).abs() > period * 0.3)
            .count();
        Some(PresentedSummary {
            frames: self.frames(),
            p50_ms: at(0.5),
            p99_ms: at(0.99),
            worst_ms: *intervals.last().unwrap() as f64 / 1_000.0,
            uneven_pct: uneven as f64 * 100.0 / intervals.len() as f64,
        })
    }
}

impl PresentedSummary {
    pub fn line(&self) -> String {
        format!(
            "{} frames shown; interval p50 {:.1} ms, p99 {:.1} ms, worst {:.1} ms; {:.1}% uneven (outside ±30%)",
            self.frames, self.p50_ms, self.p99_ms, self.worst_ms, self.uneven_pct
        )
    }
}

type SetCallback = unsafe extern "C" fn(
    *mut ndk_sys::AMediaCodec,
    ndk_sys::AMediaCodecOnFrameRendered,
    *mut std::ffi::c_void,
) -> ndk_sys::media_status_t;

unsafe extern "C" fn on_frame_rendered(
    _codec: *mut ndk_sys::AMediaCodec,
    _userdata: *mut std::ffi::c_void,
    _media_time_us: i64,
    system_ns: i64,
) {
    PRESENTED.record(system_ns);
}

/// The raw `AMediaCodec *`. The ndk crate keeps it private, but its derived
/// `Debug` prints it (`MediaCodec { inner: 0x…, … }`); without it there is
/// simply no measurement.
fn codec_pointer(codec: &MediaCodec) -> Option<*mut ndk_sys::AMediaCodec> {
    let text = format!("{codec:?}");
    let hex = text.split("inner: 0x").nth(1)?;
    let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    let address = usize::from_str_radix(&hex, 16).ok()?;
    (address != 0).then_some(address as *mut ndk_sys::AMediaCodec)
}

/// Starts measuring when frames reach the display (Android 13+; looked up
/// at run time so older releases still load the library).
pub(crate) fn measure(codec: &MediaCodec) {
    static SET: OnceLock<Option<SetCallback>> = OnceLock::new();
    let set = SET.get_or_init(|| {
        // SAFETY: libmediandk is a system library; the symbol has the
        // signature declared in <media/NdkMediaCodec.h>. The library is
        // kept loaded for the life of the process.
        unsafe {
            let library = libloading::Library::new("libmediandk.so").ok()?;
            let symbol = library
                .get::<SetCallback>(b"AMediaCodec_setOnFrameRenderedCallback\0")
                .ok()
                .map(|s| *s);
            std::mem::forget(library);
            symbol
        }
    });
    let (Some(set), Some(pointer)) = (set, codec_pointer(codec)) else {
        info!("direct video: no frame-rendered callback on this device; not measuring");
        return;
    };
    // SAFETY: `pointer` is this live codec; the callback only touches a
    // static, and the codec stops calling it when it is deleted.
    let status = unsafe { set(pointer, Some(on_frame_rendered), std::ptr::null_mut()) };
    if status == ndk_sys::media_status_t::AMEDIA_OK {
        PRESENTED.measuring.store(true, Ordering::Relaxed);
    } else {
        warn!("direct video: frame-rendered callback refused ({status:?})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_of_even_and_late_frames() {
        let stats = PresentedStats {
            frames: AtomicU64::new(0),
            last_ns: AtomicI64::new(0),
            period_us: AtomicU64::new(0),
            intervals: Mutex::new(Vec::new()),
            measuring: AtomicBool::new(false),
        };
        stats.reset(60);
        let mut t = 1_000_000_000i64;
        for i in 0..100 {
            // One frame in ten comes a whole period late.
            t += if i % 10 == 9 { 33_333_333 } else { 16_666_667 };
            stats.record(t);
        }
        let s = stats.summary().unwrap();
        assert_eq!(s.frames, 100);
        assert!((s.p50_ms - 16.7).abs() < 0.1, "{s:?}");
        assert!((s.worst_ms - 33.3).abs() < 0.1, "{s:?}");
        assert!((s.uneven_pct - 10.1).abs() < 0.5, "{s:?}");
    }
}
