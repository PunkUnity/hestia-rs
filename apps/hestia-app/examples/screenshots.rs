//! Renders the main screens headless (Slint software renderer) at phone and
//! desktop sizes, with sample data, into PNG files:
//!
//!     cargo run -p hestia-app --example screenshots -- out-dir
//!
//! CI attaches the results to the nightly release, so each build's UI can be
//! checked without a device. The software renderer is close to, but not
//! pixel-identical with, the GPU renderers the app uses (no drop shadows, so
//! no focus glow). The TV shots move the focus with the arrow keys, like a
//! remote's D-pad.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use hestia_app::{AppCard, AppWindow, CommandRow, HostCard};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, Image, ModelRc, PhysicalSize, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

fn strings(items: &[&str]) -> ModelRc<SharedString> {
    model(items.iter().map(|s| SharedString::from(*s)).collect())
}

/// A stand-in "video frame": a dark gradient with a lighter band.
fn fake_frame(width: u32, height: u32) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    for (i, px) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % width, i as u32 / width);
        let band = if (height / 3..height / 3 + height / 6).contains(&y) {
            60
        } else {
            0
        };
        *px = Rgba8Pixel {
            r: (20 + x * 60 / width + band) as u8,
            g: (30 + y * 50 / height + band) as u8,
            b: (60 + band) as u8,
            a: 255,
        };
    }
    Image::from_rgba8(buffer)
}

fn fill_sample_data(ui: &AppWindow) {
    ui.set_starting(false);
    ui.set_hosts(model(vec![
        HostCard {
            id: "a".into(),
            name: "cachy-desktop".into(),
            address: "192.168.1.20:47989".into(),
            online: true,
            checking: false,
            paired: true,
            hermes: true,
            version: "1.4".into(),
            status: "Ready to stream".into(),
            health: 1,
        },
        HostCard {
            id: "b".into(),
            name: "living-room".into(),
            address: "192.168.1.31:47989".into(),
            online: false,
            checking: false,
            paired: false,
            hermes: false,
            version: "".into(),
            status: "Last seen yesterday".into(),
            health: 0,
        },
    ]));
    ui.set_host_name("cachy-desktop".into());
    ui.set_host_detail("192.168.1.20 · Hermes 1.4".into());
    ui.set_host_chips(strings(&["HEVC", "AV1", "HDR", "Virtual display", "Gamescope"]));
    let apps = [
        "Desktop",
        "Steam Big Picture",
        "Cyberpunk 2077",
        "Hades II",
        "Elden Ring",
        "Stardew Valley",
    ];
    ui.set_apps(model(
        apps.iter()
            .enumerate()
            .map(|(i, title)| AppCard {
                id: i as i32 + 1,
                title: (*title).into(),
                art: Image::default(),
                has_art: false,
                running: i == 2,
                hdr: i == 2,
            })
            .collect(),
    ));
    ui.set_app_running(true);
    ui.set_apps_profile_options(strings(&["Settings", "Couch TV", "Phone on the go"]));
    ui.set_cfg_profile_names(strings(&["Couch TV", "Phone on the go"]));
    ui.set_client_id("6F3A9C21D4E0B7F1".into());
    ui.set_data_dir("/data/user/0/com.mroz59.hestia.rs/files".into());
    ui.set_version_text("Hestia RS 0.1.0 (preview)".into());
    ui.set_session_title("Cyberpunk 2077".into());
    ui.set_session_subtitle("cachy-desktop".into());
    ui.set_session_lines(strings(&["Connecting to the stream…"]));
    ui.set_session_busy(true);
    ui.set_stream_frame(fake_frame(320, 180));
    ui.set_stream_stats(
        "1920×1080  ·  60 fps  ·  18.4 Mbps  ·  RTT 3 ms  ·  decode 4.1 ms  ·  host 3.2 ms  ·  FEC 12  ·  lost 0  ·  dropped 0\n⚠ The network is dropping frames. Lower the bitrate, or move closer to the access point / use a wired connection. (4.1% network drops)\n3 spikes in the last ~5 min (mostly network)".into(),
    );
    // Settings as on a 2400×1080 120 Hz phone with the Balanced preset.
    ui.set_cfg_resolution_options(strings(&[
        "1280 × 720",
        "1920 × 1080",
        "2400 × 1080 (native)",
        "2560 × 1440",
        "3840 × 2160",
    ]));
    ui.set_cfg_codec_options(strings(&["Auto", "H.264", "HEVC", "AV1", "PyroWave (SDR)"]));
    ui.set_cfg_fps_options(strings(&["30 fps", "60 fps", "90 fps", "120 fps", "144 fps"]));
    ui.set_cfg_preset(2);
    ui.set_cfg_resolution(2);
    ui.set_cfg_fps(1);
    ui.set_cfg_bitrate(26.0);
    ui.set_cfg_preset_hint("This display: 2400 × 1080 at 120 Hz".into());
    ui.set_host_hermes(true);
    ui.set_tools_display_status("backend: kms · connector: HDMI-A-1 · mode: 1920x1080@60 · virtual: yes".into());
    ui.set_tools_commands(model(vec![
        CommandRow {
            id: 1,
            name: "Restart Steam".into(),
            confirm: false,
            armed: false,
        },
        CommandRow {
            id: 2,
            name: "Reboot the host".into(),
            confirm: true,
            armed: false,
        },
        CommandRow {
            id: 3,
            name: "Toggle HDR on the TV".into(),
            confirm: true,
            armed: true,
        },
    ]));
    ui.set_tools_clipboard_text("https://github.com/MrOz59/hestia-rs".into());
    ui.set_tools_permissions(
        "Allowed: view streams, list apps, launch apps, keyboard, mouse, gamepads, clipboard. Not allowed: server commands."
            .into(),
    );
    ui.set_tools_diagnostics(strings(&[
        "version: 1.4.0",
        "display.backend: kms",
        "display.hdr: no",
        "sessions: 1 items",
    ]));
    ui.set_report_title("Cyberpunk 2077 on cachy-desktop".into());
    ui.set_report_summary(strings(&[
        "Result: the app was closed on the host",
        "Duration: 12m 04s",
        "First frame after 1.8s",
        "Video: 59.6 fps average (lowest 41.0), 18.2 Mbps",
        "Network: RTT 3 ms average, max 21 ms",
        "Losses: 2 frames lost, 148 packets recovered by FEC",
        "Decoder: 4.1 ms per frame, 0 frames dropped",
    ]));
    ui.set_report_path(
        "/storage/emulated/0/Android/data/com.mroz59.hestia.rs/files/reports/stream-20261119-062410.txt".into(),
    );
    ui.set_report_log_tail(strings(&[
        "06:36:12.004 INFO  hestia_stream::session: host ended the stream (code 0x80030023)",
        "06:36:12.010 INFO  hestia_app::controller::streaming: stream ended (graceful: true)",
        "06:36:12.231 INFO  hestia_app::controller::report: stream report saved",
    ]));
    ui.set_report_can_send(true);
    ui.set_pair_host_name("cachy-desktop".into());
    ui.set_pair_pin("4821".into());
    ui.set_pair_status("Waiting for the host…".into());
    ui.set_pair_busy(true);
}

/// Lets timers fire and animations (page arrivals, dialogs, the splash
/// fading out) finish.
fn settle() {
    for _ in 0..12 {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(Duration::from_millis(50));
    }
    slint::platform::update_timers_and_animations();
}

/// Presses and releases `key`, `times` times, like a remote's D-pad.
fn press(window: &MinimalSoftwareWindow, key: Key, times: usize) {
    for _ in 0..times {
        window.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        settle();
    }
}

/// Draws the window into a fresh buffer.
fn render(window: &MinimalSoftwareWindow, size: PhysicalSize) -> Vec<Rgb8Pixel> {
    window.request_redraw();
    let (w, h) = (size.width as usize, size.height as usize);
    let mut pixels = vec![Rgb8Pixel::default(); w * h];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, w);
    });
    pixels
}

/// Draws a frame so a new screen is built (as the app would on its next
/// frame), then lets it arrive: pages focus their first control a moment
/// after they're built when the remote is in use.
fn show(window: &MinimalSoftwareWindow, size: PhysicalSize) {
    render(window, size);
    settle();
}

fn save(window: &MinimalSoftwareWindow, size: PhysicalSize, path: &Path) {
    // Animations (page fades, dialogs) start when a frame first draws
    // them: draw once, let them run, then draw the picture to keep.
    settle();
    render(window, size);
    settle();
    let pixels = render(window, size);
    let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    image::RgbImage::from_raw(size.width, size.height, bytes)
        .expect("buffer size")
        .save(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    println!("{}", path.display());
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "screenshots".into()));
    std::fs::create_dir_all(&out).expect("output directory");

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(window.clone()))).expect("platform");
    let ui = AppWindow::new().expect("window");
    fill_sample_data(&ui);
    ui.show().expect("show");

    // (name, logical width, height, scale): a 412dp phone and a desktop window.
    for (form, lw, lh, scale) in [("phone", 412u32, 915u32, 2.0f32), ("desktop", 1180, 760, 1.0)] {
        ui.set_is_android(form == "phone");
        ui.set_has_touch_input(form == "phone");
        ui.set_stream_soft_keyboard(form == "phone");
        ui.set_stream_pointer_capture_available(form == "desktop");
        window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        let size = PhysicalSize::new((lw as f32 * scale) as u32, (lh as f32 * scale) as u32);
        window.set_size(size);

        // (file name, screen, pairing dialog, stream keyboard bar)
        let shots: [(&str, i32, bool, bool); 9] = [
            ("report", 6, false, false),
            ("hosts", 0, false, false),
            ("apps", 1, false, false),
            ("launching", 2, false, false),
            ("settings", 3, false, false),
            ("pairing", 0, true, false),
            ("stream", 4, false, false),
            ("stream-keyboard", 4, false, true),
            ("tools", 5, false, false),
        ];
        for (name, screen, pairing, keyboard) in shots {
            ui.set_pair_visible(pairing);
            ui.set_stream_keyboard_open(keyboard);
            ui.set_screen(screen);
            save(&window, size, &out.join(format!("{form}-{name}.png")));
        }
        ui.set_stream_keyboard_open(false);
        ui.set_pair_visible(false);

        // A touch-capable device, including a Linux touchscreen.
        ui.set_has_touch_input(true);
        ui.set_stream_pad_controls(model(hestia_app::pad_preview(lw as f32, lh as f32)));
        ui.set_stream_pad_visible(true);
        ui.set_screen(4);
        save(&window, size, &out.join(format!("{form}-stream-pad.png")));
        ui.set_stream_pad_visible(false);
    }

    ui.set_is_android(true);
    ui.set_has_touch_input(true);
    ui.set_stream_soft_keyboard(true);
    ui.set_stream_pointer_capture_available(false);
    // Phones stream in landscape: the controller as most people see it.
    let (lw, lh, scale) = (915u32, 412u32, 2.0f32);
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
    let size = PhysicalSize::new((lw as f32 * scale) as u32, (lh as f32 * scale) as u32);
    window.set_size(size);
    ui.set_stream_pad_controls(model(hestia_app::pad_preview(lw as f32, lh as f32)));
    ui.set_stream_pad_visible(true);
    ui.set_screen(4);
    save(&window, size, &out.join("phone-landscape-stream-pad.png"));

    // The stream menu on a short landscape screen: it must scroll, with
    // Disconnect / Quit in reach.
    ui.set_stream_pad_visible(false);
    ui.set_stream_menu_open(true);
    save(&window, size, &out.join("phone-landscape-stream-menu.png"));
    ui.set_stream_menu_open(false);

    ui.set_has_touch_input(false);
    // A 1080p TV (960 × 540 dp), driven with the remote's D-pad: the focus
    // glow (an ember outline here, without the shadow) shows where it is.
    let (lw, lh, scale) = (960u32, 540u32, 2.0f32);
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
    let size = PhysicalSize::new((lw as f32 * scale) as u32, (lh as f32 * scale) as u32);
    window.set_size(size);

    ui.set_screen(0);
    show(&window, size);
    press(&window, Key::DownArrow, 1);
    save(&window, size, &out.join("tv-hosts.png"));

    // From here on the remote has been used, so pages open with the focus
    // already placed (Resume on the host's page).
    ui.set_screen(1);
    show(&window, size);
    // Down from Resume to the first poster.
    press(&window, Key::DownArrow, 1);
    save(&window, size, &out.join("tv-apps.png"));

    ui.set_screen(3);
    show(&window, size);
    // Down from Theme to the bitrate slider.
    press(&window, Key::DownArrow, 5);
    save(&window, size, &out.join("tv-settings.png"));
    // The option list of the frame rate row.
    press(&window, Key::UpArrow, 1);
    press(&window, Key::Return, 1);
    save(&window, size, &out.join("tv-settings-options.png"));
    press(&window, Key::Escape, 1);

    ui.set_screen(5);
    show(&window, size);
    press(&window, Key::DownArrow, 2);
    save(&window, size, &out.join("tv-tools.png"));

    ui.set_screen(4);
    show(&window, size);
    ui.set_stream_menu_open(true);
    settle();
    press(&window, Key::DownArrow, 2);
    save(&window, size, &out.join("tv-stream-menu.png"));
    ui.set_stream_menu_open(false);

    ui.set_screen(0);
    ui.set_pair_visible(true);
    save(&window, size, &out.join("tv-pairing.png"));
    ui.set_pair_visible(false);
}
