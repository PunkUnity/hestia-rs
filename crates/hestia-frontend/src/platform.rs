//! System services used by the graphical client (none of them depends on
//! the UI toolkit; the shell only hands over the Android activity).

#[cfg(target_os = "android")]
mod android;
mod clipboard;
mod display;
mod gamepads;
mod input;
mod keyboard;
#[cfg(target_os = "android")]
mod network;
mod reports;

#[cfg(target_os = "android")]
pub use android::{set_android_app, video_decoders};
pub use clipboard::{clipboard_stamp, clipboard_text, set_clipboard_text};
pub use display::{device_summary, native_display, set_direct_video, set_pairing_active, set_stream_mode};
#[cfg(target_os = "android")]
pub(crate) use gamepads::release_android_gamepads;
pub use gamepads::{
    gamepad_gone, gamepad_state, onscreen_gamepad, onscreen_gamepad_off, rumble, set_gamepad_sink,
    set_gamepads_streaming,
};
#[cfg(target_os = "android")]
pub use input::android_input_hook;
pub use input::{has_pointer_capture, set_key_sink, set_pointer_sink};
pub use keyboard::{has_soft_keyboard, set_ime_sink, set_keyboard_visible};
#[cfg(target_os = "android")]
pub use network::route_for;
pub use reports::{export_label, export_report, open_url, reports_dir};
