//! User-facing stream settings, persisted next to the host list.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodecPreference {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
    PyroWave,
}

impl CodecPreference {
    pub const ALL: [Self; 5] = [Self::Auto, Self::H264, Self::Hevc, Self::Av1, Self::PyroWave];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
            Self::PyroWave => "PyroWave",
        }
    }

    /// Name used by the Hestia API (`h264`, `hevc`, `av1`).
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Auto | Self::Hevc => "hevc",
            Self::H264 => "h264",
            Self::Av1 => "av1",
            Self::PyroWave => "pyrowave",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    #[default]
    Normal,
    /// Hermes: run the app inside an isolated Gamescope session.
    Gamescope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioLayout {
    #[default]
    Stereo,
    Surround51,
    Surround71,
}

/// How the video fits the window (from Hestia Android).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoScale {
    /// Whole picture, black bars if the aspect ratios differ.
    #[default]
    Fit,
    /// Fill the window, cropping the edges.
    Fill,
    /// Fill the window, distorting the picture.
    Stretch,
}

impl VideoScale {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Fill => "Fill",
            Self::Stretch => "Stretch",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Fit => Self::Fill,
            Self::Fill => Self::Stretch,
            Self::Stretch => Self::Fit,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::Fit => 0,
            Self::Fill => 1,
            Self::Stretch => 2,
        }
    }
}

/// When Android direct video shows each decoded frame (Hestia Android's
/// frame pacing modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FramePacing {
    /// One frame per display refresh, timed to vsync, with up to one frame
    /// held back to smooth over network jitter.
    #[default]
    Balanced,
    /// The newest frame goes on screen as soon as it is decoded.
    MinLatency,
    /// Every frame the decoder releases is shown, none dropped by the
    /// display.
    MaxSmoothness,
}

impl FramePacing {
    pub const ALL: [Self; 3] = [Self::Balanced, Self::MinLatency, Self::MaxSmoothness];

    pub fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::MinLatency => "Min latency",
            Self::MaxSmoothness => "Max smoothness",
        }
    }

    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|&p| p == self).unwrap_or(0) as i32
    }
}

/// What a finger on the stream does (Artemis' mouse modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TouchMode {
    /// Relative cursor, tap = click, hold = right click.
    #[default]
    Trackpad,
    /// The cursor goes where the finger is.
    Mouse,
    /// Real touches, every finger, for touch-aware apps.
    Touchscreen,
}

impl TouchMode {
    pub const ALL: [Self; 3] = [Self::Trackpad, Self::Mouse, Self::Touchscreen];

    pub fn label(self) -> &'static str {
        match self {
            Self::Trackpad => "trackpad",
            Self::Mouse => "mouse",
            Self::Touchscreen => "touchscreen",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Trackpad => Self::Mouse,
            Self::Mouse => Self::Touchscreen,
            Self::Touchscreen => Self::Trackpad,
        }
    }

    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|&m| m == self).unwrap_or(0) as i32
    }
}

/// Screen orientation while streaming (Android).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamOrientation {
    /// Landscape, either way up.
    #[default]
    Landscape,
    /// Portrait, either way up.
    Portrait,
    /// Whatever the device and its rotation lock say.
    Auto,
}

impl StreamOrientation {
    pub const ALL: [Self; 3] = [Self::Landscape, Self::Portrait, Self::Auto];

    pub fn label(self) -> &'static str {
        match self {
            Self::Landscape => "landscape",
            Self::Portrait => "portrait",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Android's request ceiling on metered connections. Zero uses the normal bitrate.
    pub metered_bitrate_kbps: u32,
    pub codec: CodecPreference,
    pub hdr: bool,
    pub audio: AudioLayout,
    pub play_audio_on_host: bool,
    /// Apollo/Hermes: stream from a virtual display matching this client.
    pub virtual_display: bool,
    /// Desktop scale in percent (Hermes `scale_factor` feature).
    pub scale_factor: u32,
    pub launch_mode: LaunchMode,
    pub optimize_game_settings: bool,
    /// Keep the clipboard in sync with the host when it supports it.
    pub clipboard_sync: bool,
    /// Touch drives the host cursor like a trackpad (relative); otherwise
    /// a touch clicks where the finger lands.
    pub touch_trackpad: bool,
    /// Show the session report (stats + logs) when a stream ends.
    pub show_stream_report: bool,
    /// Preset that produced resolution/fps/bitrate (Custom once edited).
    pub preset: crate::Preset,
    pub video_scale: VideoScale,
    /// Keep the menu button on the stream (otherwise Back/Escape open the
    /// menu). Off on phones, where the button only covers the picture.
    pub show_menu_button: bool,
    /// Use the platform's hardware decoder when there is one.
    pub hardware_decoding: bool,
    /// The window's GPU renderer draws decoded pictures itself. Android:
    /// hardware-decoded pictures go straight to the GPU (no CPU copy or
    /// conversion). Desktop: the GPU converts the software decoder's YUV to
    /// RGB. Off: pictures are converted to RGBA on the CPU.
    pub gpu_video: bool,
    /// Android: the decoder renders straight to its own surface behind the
    /// window, and the system composes it (as Moonlight does), instead of
    /// the picture going through the UI's frame. Off, or when that surface
    /// can't be set up: the zero-copy or CPU path above.
    pub direct_video: bool,
    /// How direct video paces frames to the display.
    pub frame_pacing: FramePacing,
    /// Touch mode; unset in settings saved before it existed, which only
    /// had `touch_trackpad` (see [`StreamSettings::touch`]).
    pub touch_mode: Option<TouchMode>,
    /// Show the on-screen controller over the stream.
    pub onscreen_controller: bool,
    pub orientation: StreamOrientation,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            metered_bitrate_kbps: 0,
            codec: CodecPreference::Auto,
            hdr: false,
            audio: AudioLayout::Stereo,
            play_audio_on_host: false,
            virtual_display: true,
            scale_factor: 100,
            launch_mode: LaunchMode::Normal,
            optimize_game_settings: true,
            clipboard_sync: false,
            touch_trackpad: true,
            show_stream_report: true,
            preset: crate::Preset::Custom,
            video_scale: VideoScale::Fit,
            show_menu_button: !cfg!(target_os = "android"),
            hardware_decoding: true,
            gpu_video: true,
            direct_video: true,
            frame_pacing: FramePacing::Balanced,
            touch_mode: None,
            onscreen_controller: false,
            orientation: StreamOrientation::Landscape,
        }
    }
}

impl StreamSettings {
    pub fn bitrate_for_network(&self, metered: bool) -> u32 {
        if metered && self.metered_bitrate_kbps != 0 {
            self.bitrate_kbps.min(self.metered_bitrate_kbps)
        } else {
            self.bitrate_kbps
        }
    }

    /// The touch mode, from the old trackpad switch when never chosen.
    pub fn touch(&self) -> TouchMode {
        self.touch_mode.unwrap_or(if self.touch_trackpad {
            TouchMode::Trackpad
        } else {
            TouchMode::Mouse
        })
    }

    pub fn set_touch(&mut self, mode: TouchMode) {
        self.touch_mode = Some(mode);
        self.touch_trackpad = mode == TouchMode::Trackpad;
    }

    /// Adds these settings to a session report.
    pub fn add_to_report(&self, r: &mut crate::report::StreamReport) {
        r.field(
            "Settings",
            "resolution",
            format!("{}x{} @ {} fps", self.width, self.height, self.fps),
        );
        r.field("Settings", "bitrate", format!("{} kbps", self.bitrate_kbps));
        r.field("Settings", "codec preference", self.codec.label());
        r.field("Settings", "audio", format!("{:?}", self.audio));
        r.field("Settings", "HDR", self.hdr);
        r.field("Settings", "virtual display", self.virtual_display);
        r.field("Settings", "launch mode", format!("{:?}", self.launch_mode));
        r.field("Settings", "touch", self.touch().label());
        r.field("Settings", "orientation", self.orientation.label());
        r.field(
            "Settings",
            "on-screen controller",
            if self.onscreen_controller { "on" } else { "off" },
        );
        r.field("Settings", "preset", self.preset.label());
        r.field("Settings", "video scale", self.video_scale.label());
        r.field(
            "Settings",
            "hardware decoding",
            if self.hardware_decoding { "on" } else { "off" },
        );
        let on = if self.gpu_video { "on" } else { "off" };
        if cfg!(target_os = "android") {
            r.field("Settings", "zero-copy video", on);
            r.field("Settings", "direct video", if self.direct_video { "on" } else { "off" });
            r.field("Settings", "frame pacing", self.frame_pacing.label());
        } else {
            r.field("Settings", "GPU colour conversion", on);
        }
    }

    /// Rough default bitrate (kbps) for a resolution/frame rate, in the spirit
    /// of Moonlight's defaults.
    pub fn suggested_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
        let pixels = (width as u64 * height as u64).max(1);
        let base_1080p60 = 20_000f64;
        let scale = pixels as f64 / (1920.0 * 1080.0);
        let fps_factor = (fps.max(30) as f64 / 60.0).sqrt();
        ((base_1080p60 * scale * fps_factor) as u32).clamp(5_000, 150_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let s = StreamSettings::default();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<StreamSettings>(&json).unwrap(), s);
        // Missing fields fall back to defaults.
        let partial: StreamSettings = serde_json::from_str(r#"{"fps": 120, "codec": "av1"}"#).unwrap();
        assert_eq!(partial.fps, 120);
        assert_eq!(partial.codec, CodecPreference::Av1);
        assert_eq!(partial.width, 1920);
        assert_eq!(partial.bitrate_for_network(true), partial.bitrate_kbps);
    }

    #[test]
    fn metered_bitrate_only_lowers_the_request_on_metered_networks() {
        let mut settings = StreamSettings {
            bitrate_kbps: 20_000,
            metered_bitrate_kbps: 2_500,
            ..StreamSettings::default()
        };
        assert_eq!(settings.bitrate_for_network(true), 2_500);
        assert_eq!(settings.bitrate_for_network(false), 20_000);
        settings.bitrate_kbps = 1_000;
        assert_eq!(settings.bitrate_for_network(true), 1_000);
    }

    #[test]
    fn bitrate_scales() {
        assert_eq!(StreamSettings::suggested_bitrate_kbps(1920, 1080, 60), 20_000);
        assert!(StreamSettings::suggested_bitrate_kbps(3840, 2160, 60) > 60_000);
        assert!(StreamSettings::suggested_bitrate_kbps(1280, 720, 60) < 20_000);
    }
}
