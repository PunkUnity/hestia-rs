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
//! The report separates explicit discards and releases from Android's
//! asynchronous codec render callbacks (available on Android 13+).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use log::{info, warn};
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;

use crate::DecodeStats;
pub use crate::direct_video_stats::DirectVideoStats;

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

pub(crate) fn is_current(generation: u64) -> bool {
    slot().generation == generation
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

// Keep an opaque generation token in callback userdata. A delayed callback can
// race codec teardown, so it must never dereference codec-owned memory.
static NEXT_CODEC_ID: AtomicUsize = AtomicUsize::new(1);
static CALLBACKS: LazyLock<Mutex<HashMap<usize, Weak<DecodeStats>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct Measurement {
    id: usize,
    stats: Arc<DecodeStats>,
}

impl Measurement {
    pub(crate) fn surface_changed(&self) {
        self.stats.direct_video.surface_changed(self.id, Instant::now());
    }

    pub(crate) fn codec_flushed(&self) {
        self.stats.direct_video.codec_flushed(self.id, Instant::now());
    }

    pub(crate) fn end_codec(&self) {
        self.stats.direct_video.end_codec(self.id, Instant::now());
    }

    pub(crate) fn output(&self, now: Instant) {
        self.stats.direct_video.output(self.id, now);
    }

    pub(crate) fn expect_callback(&self, pts: i64) {
        self.stats.direct_video.expect_callback(self.id, pts, Instant::now());
    }

    pub(crate) fn release_result(&self, pts: i64, visible: bool, ok: bool) {
        self.stats.direct_video.release_result(self.id, pts, visible, ok);
    }

    pub(crate) fn discard_older(&self, visible: bool) {
        self.stats.direct_video.discard_older(self.id, visible);
    }
}

impl Drop for Measurement {
    fn drop(&mut self) {
        CALLBACKS.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
        self.end_codec();
    }
}

type SetCallback = unsafe extern "C" fn(
    *mut ndk_sys::AMediaCodec,
    ndk_sys::AMediaCodecOnFrameRendered,
    *mut std::ffi::c_void,
) -> ndk_sys::media_status_t;

unsafe extern "C" fn on_frame_rendered(
    _codec: *mut ndk_sys::AMediaCodec,
    userdata: *mut std::ffi::c_void,
    media_time_us: i64,
    system_ns: i64,
) {
    let arrival = Instant::now();
    let id = userdata as usize;
    let stats = CALLBACKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&id)
        .and_then(Weak::upgrade);
    if let Some(stats) = stats {
        stats.direct_video.callback(id, media_time_us, system_ns, arrival);
    }
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

/// Observe this codec's output and register for render callbacks if supported.
pub(crate) fn measure(codec: &MediaCodec, stats: Arc<DecodeStats>, fps: u32) -> Measurement {
    let id = NEXT_CODEC_ID.fetch_add(1, Ordering::Relaxed);
    stats.direct_video.begin_codec(id, fps, Instant::now());
    let measurement = Measurement { id, stats };
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
        info!("direct video: render callbacks unavailable");
        return measurement;
    };
    CALLBACKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id, Arc::downgrade(&measurement.stats));
    // SAFETY: pointer identifies the live codec. Userdata is an opaque token,
    // never a pointer to Rust data. The registry owns only a weak reference.
    let status = unsafe { set(pointer, Some(on_frame_rendered), id as *mut std::ffi::c_void) };
    if status == ndk_sys::media_status_t::AMEDIA_OK {
        measurement.stats.direct_video.enable_callbacks(id);
    } else {
        CALLBACKS.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        warn!("direct video: frame-rendered callback refused ({status:?})");
    }
    measurement
}
