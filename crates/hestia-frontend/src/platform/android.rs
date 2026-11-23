//! Android activity, JNI, and the shared Java helper.

use std::sync::{OnceLock, RwLock};

use super::keyboard::ime_event;
use crate::stream_input::ImeEvent;

static ANDROID_APP: RwLock<Option<android_activity::AndroidApp>> = RwLock::new(None);

/// The activity every Android service here goes through (JNI, the Java
/// helper); the shell hands it over before starting the controller.
pub fn set_android_app(app: android_activity::AndroidApp) {
    *ANDROID_APP.write().unwrap() = Some(app);
}

pub(super) fn android_app() -> Option<android_activity::AndroidApp> {
    ANDROID_APP.read().unwrap().clone()
}

/// Runs `f` with a JNI environment and the Activity, clearing any Java
/// exception it leaves behind (a pending one would abort the next JNI call).
pub(super) fn with_activity(
    f: impl FnOnce(&mut jni::JNIEnv<'_>, &jni::objects::JObject<'_>) -> jni::errors::Result<()>,
) -> Result<(), String> {
    use jni::JavaVM;
    use jni::objects::JObject;

    let app = android_app().ok_or("Android context not available")?;
    // SAFETY: android-activity hands out the process' JavaVM and a global
    // reference to the Activity, both valid for the life of the app.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let result = f(&mut *env, &activity);
    if result.is_err() && env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.map_err(|e| e.to_string())
}

static HELPER: OnceLock<jni::objects::GlobalRef> = OnceLock::new();

/// Runs `f` with HestiaHelper's class, loading it on first use.
pub(super) fn with_helper(
    f: impl FnOnce(&mut jni::JNIEnv<'_>, &jni::objects::JObject<'_>, &jni::objects::JClass<'_>) -> jni::errors::Result<()>,
) -> Result<(), String> {
    with_activity(|env, activity| {
        if HELPER.get().is_none() {
            let class = load_helper(env, activity)?;
            let _ = HELPER.set(class);
        }
        let Some(global) = HELPER.get() else {
            return Ok(());
        };
        let class = jni::objects::JClass::from(env.new_local_ref(global.as_obj())?);
        f(env, activity, &class)
    })
}

/// Candidates ordered for Auto. Java can verify size/rate and hardware
/// support on the Android API levels covered by the app.
pub fn video_decoders(width: u32, height: u32, fps: u32) -> Result<Vec<(hestia_media::VideoCodec, String)>, String> {
    use hestia_media::VideoCodec;
    use jni::objects::{JString, JValue};

    let mut decoders = Vec::new();
    with_helper(|env, _, helper| {
        for (codec, mime) in [
            (VideoCodec::Hevc, "video/hevc"),
            (VideoCodec::H264, "video/avc"),
            (VideoCodec::Av1, "video/av01"),
        ] {
            let mime = env.new_string(mime)?;
            let name = env
                .call_static_method(
                    helper,
                    "findVideoDecoder",
                    "(Ljava/lang/String;III)Ljava/lang/String;",
                    &[
                        JValue::Object(&mime),
                        JValue::Int(width as i32),
                        JValue::Int(height as i32),
                        JValue::Int(fps as i32),
                    ],
                )?
                .l()?;
            if !name.is_null() {
                let name = env.get_string(&JString::from(name))?.into();
                decoders.push((codec, name));
            }
        }
        Ok(())
    })?;
    Ok(decoders)
}

/// Loads HestiaHelper from the dex built by build.rs (embedded here) and
/// registers its native methods.
fn load_helper(
    env: &mut jni::JNIEnv<'_>,
    activity: &jni::objects::JObject<'_>,
) -> jni::errors::Result<jni::objects::GlobalRef> {
    use jni::objects::{JClass, JValue};

    static DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/classes.dex"));
    // SAFETY: DEX is static and never written; the class loader only reads it.
    let buffer = unsafe { env.new_direct_byte_buffer(DEX.as_ptr().cast_mut(), DEX.len())? };
    let parent = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let loader = env.new_object(
        "dalvik/system/InMemoryDexClassLoader",
        "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
        &[JValue::Object(&buffer), JValue::Object(&parent)],
    )?;
    let name = env.new_string("com.mroz59.hestia.HestiaHelper")?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let class = JClass::from(class);
    let methods = [
        jni::NativeMethod {
            name: "nativeCommitText".into(),
            sig: "(Ljava/lang/String;)V".into(),
            fn_ptr: native_commit_text as *mut std::ffi::c_void,
        },
        jni::NativeMethod {
            name: "nativeDeleteBefore".into(),
            sig: "(I)V".into(),
            fn_ptr: native_delete_before as *mut std::ffi::c_void,
        },
        jni::NativeMethod {
            name: "nativeVideoSurface".into(),
            sig: "(Landroid/view/Surface;)V".into(),
            fn_ptr: native_video_surface as *mut std::ffi::c_void,
        },
    ];
    env.register_native_methods(&class, &methods)?;
    log::info!("Android helper loaded");
    env.new_global_ref(class)
}

extern "system" fn native_commit_text(
    mut env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    text: jni::objects::JString<'_>,
) {
    if let Ok(text) = env.get_string(&text) {
        ime_event(ImeEvent::Text(text.into()));
    }
}

extern "system" fn native_delete_before(
    _env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    count: jni::sys::jint,
) {
    if count > 0 {
        ime_event(ImeEvent::DeleteBefore(count as usize));
    }
}

/// The direct video surface appeared or went away (UI thread; the decoder
/// thread picks it up).
extern "system" fn native_video_surface(
    env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    surface: jni::objects::JObject<'_>,
) {
    // SAFETY: called by the VM on this thread with its JNIEnv and a local
    // reference to a Surface, or null.
    unsafe {
        hestia_media::direct_video::set_surface_from_java(env.get_raw().cast(), surface.as_raw().cast());
    }
}
