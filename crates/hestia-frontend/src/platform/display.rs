//! Display capabilities and the system window mode used for streaming.

#[cfg(target_os = "android")]
use super::android::{with_activity, with_helper};

/// This device's display as `(width, height, refresh)`, landscape, for the
/// streaming presets. Looked up once; `None` when it can't be told.
pub fn native_display() -> Option<(u32, u32, u32)> {
    static NATIVE: std::sync::OnceLock<Option<(u32, u32, u32)>> = std::sync::OnceLock::new();
    *NATIVE.get_or_init(|| {
        let found = detect_display().map(|(w, h, hz)| (w.max(h), w.min(h), hz));
        log::info!("native display: {found:?}");
        found
    })
}

/// Largest connected screen, from the kernel's DRM connectors: the EDID's
/// preferred mode, or the first listed mode at 60 Hz without one.
#[cfg(not(target_os = "android"))]
fn detect_display() -> Option<(u32, u32, u32)> {
    let mut best: Option<(u32, u32, u32)> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let dir = entry.path();
        let connected = std::fs::read_to_string(dir.join("status")).is_ok_and(|s| s.trim() == "connected");
        if !connected {
            continue;
        }
        let from_edid = std::fs::read(dir.join("edid"))
            .ok()
            .and_then(|edid| hestia_core::presets::edid_preferred_mode(&edid));
        let mode = from_edid.or_else(|| {
            let modes = std::fs::read_to_string(dir.join("modes")).ok()?;
            let (w, h) = modes.lines().next()?.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().trim_end_matches('i').parse().ok()?, 60))
        });
        if let Some(mode) = mode
            && best.is_none_or(|b| u64::from(mode.0) * u64::from(mode.1) > u64::from(b.0) * u64::from(b.1))
        {
            best = Some(mode);
        }
    }
    best
}

/// `Display.getMode()`: the physical panel size, with the highest refresh
/// rate the panel offers at that size.
#[cfg(target_os = "android")]
fn detect_display() -> Option<(u32, u32, u32)> {
    let mut found = None;
    let result = with_activity(|env, activity| {
        let wm = env
            .call_method(activity, "getWindowManager", "()Landroid/view/WindowManager;", &[])?
            .l()?;
        let display = env
            .call_method(&wm, "getDefaultDisplay", "()Landroid/view/Display;", &[])?
            .l()?;
        let mode = env
            .call_method(&display, "getMode", "()Landroid/view/Display$Mode;", &[])?
            .l()?;
        let width = env.call_method(&mode, "getPhysicalWidth", "()I", &[])?.i()?;
        let height = env.call_method(&mode, "getPhysicalHeight", "()I", &[])?.i()?;
        let mut refresh = env.call_method(&mode, "getRefreshRate", "()F", &[])?.f()?;
        let modes = env
            .call_method(&display, "getSupportedModes", "()[Landroid/view/Display$Mode;", &[])?
            .l()?;
        let modes = jni::objects::JObjectArray::from(modes);
        for i in 0..env.get_array_length(&modes)? {
            let other = env.get_object_array_element(&modes, i)?;
            let w = env.call_method(&other, "getPhysicalWidth", "()I", &[])?.i()?;
            let h = env.call_method(&other, "getPhysicalHeight", "()I", &[])?.i()?;
            if w == width && h == height {
                refresh = refresh.max(env.call_method(&other, "getRefreshRate", "()F", &[])?.f()?);
            }
            env.delete_local_ref(other)?;
        }
        if width > 0 && height > 0 {
            found = Some((width as u32, height as u32, refresh.round().max(1.0) as u32));
        }
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("reading the display mode failed: {e}");
    }
    found
}

/// The phone or tablet, for reports: "samsung SM-X710, Android 14 (SDK 34),
/// SoC QTI SM8550 (kalama)". None off Android.
pub fn device_summary() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        fn prop(name: &str) -> String {
            use std::ffi::{CStr, CString, c_char, c_int};
            unsafe extern "C" {
                fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
            }
            let Ok(name) = CString::new(name) else {
                return String::new();
            };
            // PROP_VALUE_MAX is 92 bytes, terminator included.
            let mut value = [0 as c_char; 92];
            // SAFETY: a NUL-terminated name and a PROP_VALUE_MAX buffer.
            let len = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
            if len <= 0 {
                return String::new();
            }
            // SAFETY: bionic NUL-terminates the value within the buffer.
            unsafe { CStr::from_ptr(value.as_ptr()) }
                .to_string_lossy()
                .trim()
                .to_owned()
        }
        let join = |parts: &[String]| {
            parts
                .iter()
                .filter(|p| !p.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let device = join(&[prop("ro.product.manufacturer"), prop("ro.product.model")]);
        let soc = join(&[prop("ro.soc.manufacturer"), prop("ro.soc.model")]);
        let board = prop("ro.board.platform");
        let soc = match (soc.is_empty(), board.is_empty()) {
            (false, false) => format!(", SoC {soc} ({board})"),
            (false, true) => format!(", SoC {soc}"),
            (true, false) => format!(", SoC {board}"),
            (true, true) => String::new(),
        };
        Some(format!(
            "{device}, Android {} (SDK {}){soc}",
            prop("ro.build.version.release"),
            prop("ro.build.version.sdk")
        ))
    }
    #[cfg(not(target_os = "android"))]
    None
}

/// Streaming on/off: on Android, the chosen orientation, system bars
/// hidden, screen kept on. Nothing on desktop (the window's full screen is
/// its own toggle).
pub fn set_stream_mode(on: bool, orientation: hestia_core::StreamOrientation) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            let orientation = match orientation {
                hestia_core::StreamOrientation::Landscape => 0,
                hestia_core::StreamOrientation::Portrait => 1,
                hestia_core::StreamOrientation::Auto => 2,
            };
            env.call_static_method(
                helper,
                "setStreamMode",
                "(Landroid/app/Activity;ZI)V",
                &[
                    JValue::Object(activity),
                    JValue::Bool(u8::from(on)),
                    JValue::Int(orientation),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("switching stream mode failed: {e}");
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = (on, orientation);
}

/// Android direct video on/off: the helper adds (or removes) the surface
/// behind the window that the decoder renders to, sized to the stream.
pub fn set_direct_video(on: bool, width: u32, height: u32, fps: u32) {
/// Android PIN pairing in progress: a foreground service keeps the app's
/// network while the user is in the browser typing the PIN.
pub fn set_pairing_active(on: bool) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            env.call_static_method(
                helper,
<<<<<<< HEAD
                "setDirectVideo",
                "(Landroid/app/Activity;ZIII)V",
                &[
                    JValue::Object(activity),
                    JValue::Bool(u8::from(on)),
                    JValue::Int(width as i32),
                    JValue::Int(height as i32),
                    JValue::Int(fps as i32),
                ],
=======
                "setPairingService",
                "(Landroid/app/Activity;Z)V",
                &[JValue::Object(activity), JValue::Bool(u8::from(on))],
>>>>>>> fix/android-pairing-background
            )?;
            Ok(())
        });
        if let Err(e) = result {
<<<<<<< HEAD
            log::warn!("switching direct video failed: {e}");
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = (on, width, height, fps);
=======
            log::warn!("pairing service: {e}");
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = on;
>>>>>>> fix/android-pairing-background
}
