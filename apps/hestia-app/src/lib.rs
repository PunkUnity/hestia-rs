//! Hestia RS's graphical client, drawn with Slint.
//!
//! This crate is the UI shell only: the screens (`ui/*.slint`), the Slint
//! side of the UI port (`slint_ui`), and the GPU video renderers that hook
//! into Slint's rendering (`gl_video` on the desktop, `gpu_render` on
//! Android). Everything else — the controller, input, system services, video
//! presentation — is `hestia-frontend`, which knows no toolkit; see
//! `docs/FRONTENDS.md` for writing another shell.

#[cfg(target_os = "android")]
mod android_pip;
#[cfg(not(target_os = "android"))]
mod desktop_input;
#[cfg(not(target_os = "android"))]
mod gl_video;
#[cfg(target_os = "android")]
mod gpu_render;
mod slint_ui;

use std::path::PathBuf;
use std::sync::Arc;

use hestia_core::store::{AppearanceTheme, Store};
use hestia_frontend::Controller;
use hestia_frontend::presentation::GpuWindow;

slint::include_modules!();

/// The on-screen controller, untouched, for a `width`×`height` stream view
/// (the screenshot example draws it).
#[doc(hidden)]
pub fn pad_preview(width: f32, height: f32) -> Vec<PadControl> {
    hestia_frontend::pad_preview(width, height)
        .into_iter()
        .map(slint_ui::pad_control)
        .collect()
}

/// Builds the window and runs the event loop until it closes.
#[cfg(not(target_os = "android"))]
pub fn run(data_dir: PathBuf) -> Result<(), slint::PlatformError> {
    run_window(data_dir)
}

fn run_window(
    data_dir: PathBuf,
    #[cfg(target_os = "android")] app: &slint::android::AndroidApp,
) -> Result<(), slint::PlatformError> {
    // Desktop: keys by position, every finger, mouse capture (winit, below
    // Slint). Must come before the window.
    #[cfg(not(target_os = "android"))]
    desktop_input::install();
    let ui = AppWindow::new()?;
    let appearance_dir = data_dir.clone();
    match Store::open(&appearance_dir).and_then(|store| store.load_theme()) {
        Ok(theme) => ui.set_saved_theme(i32::from(theme == AppearanceTheme::Daylight)),
        Err(e) => log::warn!("could not load appearance: {e}"),
    }
    ui.on_save_theme(move |index| {
        let theme = if index == 1 {
            AppearanceTheme::Daylight
        } else {
            AppearanceTheme::Hearth
        };
        match Store::open(&appearance_dir).and_then(|store| store.save_theme(theme)) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("could not save appearance: {e}");
                false
            }
        }
    });
    ui.set_is_android(cfg!(target_os = "android"));
    #[cfg(target_os = "android")]
    ui.set_has_touch_input(!matches!(
        app.config().touchscreen(),
        slint::android::android_activity::ndk::configuration::Touchscreen::NoTouch
    ));
    // The renderer that draws native pictures inside Slint's rendering.
    let gpu_window = Arc::new(GpuWindow::default());
    #[cfg(target_os = "android")]
    gpu_render::install(&ui, gpu_window.clone());
    #[cfg(not(target_os = "android"))]
    gl_video::install(&ui, gpu_window.clone());
    let controller = Controller::new(Arc::new(slint_ui::SlintUi::new(&ui)), gpu_window);
    slint_ui::wire(&ui, &controller);
    #[cfg(target_os = "android")]
    let _pip = android_pip::install(app, &ui, &controller);
    #[cfg(not(target_os = "android"))]
    desktop_input::connect(&controller);
    // Desktop: game controllers straight from evdev.
    #[cfg(target_os = "linux")]
    hestia_frontend::desktop_pads::start();
    controller.start(data_dir);
    let result = ui.run();
    #[cfg(target_os = "android")]
    controller.stream_disconnect();
    result
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    // logcat as before; the capture keeps lines for stream reports.
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("hestia"),
    );
    let _ = hestia_core::logcapture::install(Box::new(logcat), log::LevelFilter::Info);
    hestia_frontend::platform::set_android_app(app.clone());
    // Reach LAN hosts over Wi-Fi even when Android prefers mobile data.
    hestia_core::nvhttp::http::set_route_hook(hestia_frontend::platform::route_for);
    // Game controllers, keyboards by position, every finger and pens reach
    // the host through the (patched) backend's hook.
    i_slint_backend_android_activity::set_input_hook(|event| {
        android_pip::blocks_input() || hestia_frontend::platform::android_input_hook(event)
    });
    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp/hestia-rs"));
    if let Err(e) = slint::android::init(app.clone()) {
        log::error!("failed to initialise Slint on Android: {e}");
        return;
    }
    if let Err(e) = run_window(data_dir, &app) {
        log::error!("Hestia exited with an error: {e}");
    }
}
