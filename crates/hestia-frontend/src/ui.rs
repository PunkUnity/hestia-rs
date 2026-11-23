//! The port between the app and a UI toolkit.
//!
//! The controller never names a toolkit. It talks to the screen through two
//! traits that each shell (today Slint, in `apps/hestia-app`) implements:
//!
//! * [`UiHandle`]: thread-safe; queues work for the UI thread, now or after a
//!   delay. Worker threads use it to hand results to the screen.
//! * [`View`]: what the screens show, as plain Rust values (strings, numbers,
//!   the model structs below). It is only used on the UI thread, inside a
//!   closure given to [`UiHandle`].
//!
//! Going the other way, the shell turns the user's actions into calls on
//! [`Controller`](crate::controller::Controller)'s public methods (its
//! intents), and draws video from [`presentation`](crate::presentation).
//!
//! A retained-mode toolkit (Slint, Qt) maps each setter to a property; an
//! immediate-mode one (egui) stores the values in its own state struct, reads
//! them back in the getters, and asks for a repaint. See `docs/FRONTENDS.md`.

use std::sync::Arc;
use std::time::Duration;

use hestia_media::DecodedFrame;

pub use hestia_core::stream::input::Modifiers;

/// Work for the UI thread, with the window's view.
pub type UiTask = Box<dyn FnOnce(&dyn View) + Send>;

/// Runs code on the toolkit's UI thread. Cheap to clone behind an `Arc`.
pub trait UiHandle: Send + Sync + 'static {
    /// Queues `task` for the UI thread. Dropped when the window is gone.
    fn run(&self, task: UiTask);
    /// Queues `task` for the UI thread once `delay` has passed (toasts,
    /// long presses, the click of a stick tap).
    fn run_after(&self, delay: Duration, task: UiTask);
}

/// Which page the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Hosts,
    Apps,
    /// Launching an app, and launch errors.
    Session,
    Settings,
    Stream,
    /// Hermes host tools.
    Tools,
    /// The last stream report.
    Report,
}

impl Screen {
    pub const ALL: [Screen; 7] = [
        Screen::Hosts,
        Screen::Apps,
        Screen::Session,
        Screen::Settings,
        Screen::Stream,
        Screen::Tools,
        Screen::Report,
    ];

    /// The page's number, in [`Screen::ALL`] order.
    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|&s| s == self).unwrap_or(0) as i32
    }

    pub fn from_index(index: i32) -> Self {
        usize::try_from(index)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or_default()
    }
}

/// A host on the hosts page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostCard {
    pub id: String,
    pub name: String,
    pub address: String,
    pub online: bool,
    pub checking: bool,
    pub paired: bool,
    pub hermes: bool,
    pub version: String,
    pub status: String,
    /// Hermes readiness preflight: 0 unknown, 1 ready, 2 warnings, 3 not ready.
    pub health: i32,
}

/// An RGBA8 picture (box art), row-major without padding.
#[derive(Debug, Clone, PartialEq)]
pub struct Art {
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// An app on the apps page; box art arrives later (see [`View::set_app_art`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppCard {
    pub id: i32,
    pub title: String,
    pub art: Option<Art>,
    pub running: bool,
    pub hdr: bool,
}

/// One line of a Hermes host's readiness preflight.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreflightRow {
    /// 1 ok, 2 warning, 3 failure.
    pub level: i32,
    pub title: String,
    pub message: String,
}

/// A Hermes server command on the tools page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommandRow {
    pub id: i32,
    pub name: String,
    /// Hermes wants a confirmation before running it.
    pub confirm: bool,
    /// First tap done; the next one runs it.
    pub armed: bool,
}

/// One control of the on-screen game controller, laid out in the stream
/// view's coordinates (logical pixels).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PadControl {
    /// 0 round button, 1 rectangle (bumper, trigger, menu), 2 stick, 3 d-pad.
    pub kind: i32,
    pub label: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub pressed: bool,
    pub knob_x: f32,
    pub knob_y: f32,
    /// D-pad: held directions.
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
}

/// The settings form as the user edits it: choices are indices into the
/// option lists (see `view::settings_form` and `view::settings_from_form`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsForm {
    pub resolution_options: Vec<String>,
    pub fps_options: Vec<String>,
    pub preset: i32,
    pub preset_hint: String,
    pub video_scale: i32,
    pub resolution: i32,
    pub fps: i32,
    pub bitrate_mbps: f32,
    pub codec: i32,
    pub audio: i32,
    pub hdr: bool,
    pub virtual_display: bool,
    pub host_audio: bool,
    pub gamescope: bool,
    pub optimize: bool,
    pub clipboard: bool,
    pub show_report: bool,
    pub show_menu_button: bool,
    pub hardware_decoding: bool,
    pub gpu_video: bool,
    pub direct_video: bool,
    pub orientation: i32,
}

/// Everything the screens show, set from the UI thread. Setters replace a
/// value; getters return what the screen holds now (the user may have
/// changed it, e.g. a form field or the stats toggle).
pub trait View {
    // Window ------------------------------------------------------------
    fn set_screen(&self, screen: Screen);
    fn screen(&self) -> Screen;
    /// The window's size in physical pixels.
    fn window_size(&self) -> (u32, u32);
    fn set_window_fullscreen(&self, on: bool);
    fn set_toast(&self, text: String);
    fn toast(&self) -> String;

    // Startup and About --------------------------------------------------
    fn set_starting(&self, on: bool);
    fn set_starting_text(&self, text: String);
    fn set_version_text(&self, text: String);
    fn set_client_id(&self, id: String);
    fn set_data_dir(&self, path: String);

    // Hosts --------------------------------------------------------------
    fn set_hosts(&self, hosts: Vec<HostCard>);
    fn set_scanning(&self, on: bool);
    fn set_add_visible(&self, on: bool);
    fn set_add_busy(&self, on: bool);
    fn set_add_error(&self, text: String);
    fn set_add_address(&self, text: String);

    // Pairing ------------------------------------------------------------
    fn set_pair_visible(&self, on: bool);
    fn set_pair_host_name(&self, text: String);
    fn set_pair_pin(&self, text: String);
    fn set_pair_status(&self, text: String);
    fn set_pair_busy(&self, on: bool);
    fn set_pair_failed(&self, on: bool);
    fn set_pair_otp_mode(&self, on: bool);
    fn set_otp_pin(&self, text: String);
    fn set_otp_passphrase(&self, text: String);

    // Apps ---------------------------------------------------------------
    fn set_host_name(&self, text: String);
    fn set_host_detail(&self, text: String);
    fn set_host_chips(&self, chips: Vec<String>);
    fn set_host_hermes(&self, on: bool);
    fn set_apps(&self, apps: Vec<AppCard>);
    /// Box art for the card at `index`, if it is still the app `app_id`.
    fn set_app_art(&self, index: usize, app_id: i32, art: Art);
    /// Marks the card of `app_id` as running and every other one as not
    /// (0: none runs).
    fn set_running_app(&self, app_id: u32);
    fn set_apps_loading(&self, on: bool);
    fn set_app_running(&self, on: bool);
    fn set_apps_profile_options(&self, options: Vec<String>);
    fn set_apps_profile_index(&self, index: i32);

    // Launch session -----------------------------------------------------
    fn set_session_title(&self, text: String);
    fn set_session_subtitle(&self, text: String);
    fn set_session_lines(&self, lines: Vec<String>);
    fn set_session_busy(&self, on: bool);

    // Stream report ------------------------------------------------------
    fn set_report_title(&self, text: String);
    fn set_report_summary(&self, lines: Vec<String>);
    fn set_report_path(&self, text: String);
    fn set_report_log_tail(&self, lines: Vec<String>);
    fn set_report_export_label(&self, text: String);
    fn set_report_can_send(&self, on: bool);
    fn set_report_busy(&self, on: bool);

    // Host tools ---------------------------------------------------------
    fn set_tools_loading(&self, on: bool);
    fn set_tools_busy(&self, on: bool);
    fn set_tools_message(&self, text: String);
    fn set_tools_display_status(&self, text: String);
    fn set_tools_display_allowed(&self, on: bool);
    fn set_tools_commands(&self, commands: Vec<CommandRow>);
    fn set_tools_commands_allowed(&self, on: bool);
    fn set_tools_clipboard_allowed(&self, on: bool);
    fn set_tools_clipboard_text(&self, text: String);
    fn set_tools_permissions(&self, text: String);
    fn set_tools_diagnostics(&self, lines: Vec<String>);
    fn set_tools_status_lines(&self, lines: Vec<String>);
    fn set_tools_preflight(&self, rows: Vec<PreflightRow>);

    // Stream -------------------------------------------------------------
    /// Shows an RGBA picture (`frame.rgba()` is `Some`). Pictures the
    /// window's GPU renderer draws itself only need [`View::request_redraw`].
    fn show_video_frame(&self, frame: &DecodedFrame);
    /// Removes the stream picture (stream ended, or a new one starts).
    fn clear_video(&self);
    /// Asks the toolkit to render the window again soon.
    fn request_redraw(&self);
    fn set_stream_stats(&self, text: String);
    fn set_stream_status(&self, text: String);
    /// The stream's picture is on the direct video surface behind the
    /// window: the stream view leaves its area clear.
    fn set_stream_direct_video(&self, _on: bool) {}
    fn set_stream_show_stats(&self, on: bool);
    fn stream_show_stats(&self) -> bool;
    fn set_stream_fullscreen(&self, on: bool);
    fn stream_fullscreen(&self) -> bool;
    /// `TouchMode` index.
    fn set_stream_touch_mode(&self, mode: i32);
    fn set_stream_menu_open(&self, on: bool);
    fn set_stream_pad_visible(&self, on: bool);
    fn set_stream_pad_controls(&self, controls: Vec<PadControl>);
    fn set_stream_clipboard(&self, on: bool);
    fn set_stream_zoom_mode(&self, on: bool);
    fn set_stream_zoom(&self, factor: f32);
    fn set_stream_pan_x(&self, x: f32);
    fn set_stream_pan_y(&self, y: f32);
    fn set_stream_keyboard_open(&self, on: bool);
    fn stream_keyboard_open(&self) -> bool;
    fn set_stream_banner(&self, text: String);
    fn stream_banner(&self) -> String;
    fn set_stream_show_menu_button(&self, on: bool);
    fn set_stream_soft_keyboard(&self, on: bool);
    /// `VideoScale` index.
    fn set_stream_video_scale(&self, scale: i32);
    /// The stream menu's Mouse entry: captured (relative motion) or free.
    fn set_stream_mouse_capture(&self, on: bool);
    /// Whether the menu offers mouse capture (desktop).
    fn set_stream_pointer_capture_available(&self, on: bool);
    /// Locks the window's pointer in place and hides it (on), or frees it
    /// (off). While locked, the shell feeds relative motion to
    /// `Controller::stream_mouse_motion`.
    fn set_pointer_capture(&self, on: bool);

    // Settings -----------------------------------------------------------
    fn show_settings(&self, form: SettingsForm);
    fn settings_form(&self) -> SettingsForm;
    /// `VideoScale` index, without reloading the rest of the form.
    fn set_cfg_video_scale(&self, scale: i32);
    fn set_cfg_profile_names(&self, names: Vec<String>);
    fn set_cfg_profile_pick(&self, index: i32);
    fn cfg_profile_pick(&self) -> i32;
}

#[cfg(test)]
mod tests {
    use super::Screen;

    #[test]
    fn screens_round_trip_their_index() {
        for (i, screen) in Screen::ALL.into_iter().enumerate() {
            assert_eq!(screen.index(), i as i32);
            assert_eq!(Screen::from_index(i as i32), screen);
        }
        assert_eq!(Screen::from_index(-1), Screen::Hosts);
        assert_eq!(Screen::from_index(99), Screen::Hosts);
        // The page numbers app.slint uses.
        assert_eq!(Screen::Settings.index(), 3);
        assert_eq!(Screen::Report.index(), 6);
    }
}
