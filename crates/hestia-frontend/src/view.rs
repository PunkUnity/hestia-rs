//! Mapping between core types and the port's view models (`crate::ui`).

use hestia_core::nvhttp::{AppInfo, codecs};
use hestia_core::{
    AudioLayout, CodecPreference, HostRecord, HostSnapshot, LaunchMode, Preset, StreamSettings, VideoScale,
};

use crate::ui::{AppCard, HostCard, SettingsForm, View};

pub const RESOLUTIONS: [(u32, u32); 4] = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];
pub const FRAME_RATES: [u32; 5] = [30, 60, 90, 120, 144];
pub const VIDEO_SCALES: [VideoScale; 3] = [VideoScale::Fit, VideoScale::Fill, VideoScale::Stretch];

/// Resolution and frame-rate choices: the common ones, this display's
/// native mode and whatever is currently set (a preset may pick an
/// uncommon size), narrowest first.
pub fn mode_options(native: Option<(u32, u32, u32)>, s: &StreamSettings) -> (Vec<(u32, u32)>, Vec<u32>) {
    let mut sizes: Vec<(u32, u32)> = RESOLUTIONS.to_vec();
    let mut rates: Vec<u32> = FRAME_RATES.to_vec();
    if let Some((w, h, hz)) = native {
        sizes.push((w, h));
        rates.push(hz);
    }
    sizes.push((s.width, s.height));
    rates.push(s.fps);
    sizes.sort_unstable();
    sizes.dedup();
    rates.sort_unstable();
    rates.dedup();
    (sizes, rates)
}

pub fn resolution_label((w, h): (u32, u32), native: Option<(u32, u32, u32)>) -> String {
    if native.is_some_and(|(nw, nh, _)| (nw, nh) == (w, h)) {
        format!("{w} × {h} (native)")
    } else {
        format!("{w} × {h}")
    }
}

/// The numbers in a label, e.g. "2400 × 1080 (native)" → [2400, 1080].
fn numbers(label: &str) -> Vec<u32> {
    label
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// "2560x1080" (also with ×, *, spaces) → an encoder-friendly size: both
/// sides even and within what hosts accept.
pub fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split(['x', 'X', '×', '*']).map(|p| p.trim().parse::<u32>().ok());
    let (Some(Some(w)), Some(Some(h)), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    ((320..=7680).contains(&w) && (200..=4320).contains(&h)).then_some((w & !1, h & !1))
}

/// Kbps from the slider's Mbps, in 250 kbps steps (presets use quarters).
pub fn bitrate_from_mbps(mbps: f32) -> u32 {
    (((mbps * 4.0).round() / 4.0) * 1000.0).round().clamp(500.0, 500_000.0) as u32
}

pub fn preset_hint(native: Option<(u32, u32, u32)>) -> String {
    match native {
        Some((w, h, hz)) => format!("This display: {w} × {h} at {hz} Hz"),
        None => "This display's mode is unknown; presets assume 1920 × 1080 at 60 Hz".to_owned(),
    }
}
pub const AUDIO_LAYOUTS: [AudioLayout; 3] = [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71];

pub fn host_card(record: &HostRecord, snapshot: Option<&HostSnapshot>, checking: bool) -> HostCard {
    let online = snapshot.is_some_and(|s| s.online);
    let paired = snapshot.map_or(
        record.is_paired(),
        |s| if s.online { s.paired } else { record.is_paired() },
    );
    let caps = snapshot.and_then(|s| s.capabilities.as_ref());
    // Hermes preflight: only meaningful for a paired, reachable host.
    let (health, health_text) = match snapshot {
        Some(s) if s.online && s.paired => crate::hostinfo::health(s.diagnostics.as_ref()),
        _ => (0, None),
    };
    let status = match snapshot {
        None if checking => "Contacting host…".to_owned(),
        None => String::new(),
        Some(s) if !s.online => s.error.clone().unwrap_or_else(|| "Offline".into()),
        Some(s) => {
            let busy = s.info.as_ref().is_some_and(|i| i.is_busy());
            if !s.paired && record.is_paired() {
                "The host no longer recognises this device. Tap to pair again.".into()
            } else if !s.paired {
                "Tap to pair".into()
            } else if let Some(e) = s.info.as_ref().and_then(|i| i.secure_error.as_ref()) {
                format!("Paired, but the host's secure port isn't answering (busy starting an app?): {e}")
            } else if busy {
                "An app is running".into()
            } else {
                health_text.unwrap_or_else(|| "Ready".into())
            }
        }
    };
    HostCard {
        id: record.id.clone(),
        name: if record.name.is_empty() {
            record.address.clone()
        } else {
            record.name.clone()
        },
        address: record.active_address().to_string(),
        online,
        checking: checking && snapshot.is_none(),
        paired,
        hermes: caps.is_some(),
        version: caps.map(|c| c.server_version.clone()).unwrap_or_default(),
        status,
        health,
    }
}

/// Short capability labels shown under the host name on the apps page.
pub fn host_chips(snapshot: &HostSnapshot) -> Vec<String> {
    let mut chips: Vec<String> = Vec::new();
    match &snapshot.capabilities {
        Some(caps) => {
            let f = &caps.features;
            chips.push(if caps.server_version.is_empty() {
                "Hermes".into()
            } else {
                format!("Hermes {}", caps.server_version)
            });
            if f.virtual_display {
                let backends = f.virtual_display_backend.join("/").replace("hermes_kms", "Hermes-KMS");
                chips.push(if backends.is_empty() {
                    "Virtual display".into()
                } else {
                    format!("Virtual display · {backends}")
                });
            }
            if f.hdr_mode_control {
                chips.push("HDR".into());
            }
            if f.gamescope_session {
                chips.push("Gamescope".into());
            }
            if f.clipboard_sync {
                chips.push("Clipboard".into());
            }
            if caps.limits.max_width > 0 {
                chips.push(format!(
                    "Up to {}×{} @ {}",
                    caps.limits.max_width, caps.limits.max_height, caps.limits.max_fps
                ));
            }
        }
        None => chips.push("GameStream host".into()),
    }
    if let Some(info) = &snapshot.info {
        chips.extend(codecs::describe(info.codec_mode_support).into_iter().map(str::to_owned));
    }
    chips
}

pub fn app_card(app: &AppInfo, running_app: u32) -> AppCard {
    AppCard {
        id: app.id as i32,
        title: app.title.clone(),
        art: None,
        running: running_app != 0 && running_app == app.id,
        hdr: app.hdr_supported,
    }
}

/// Decodes box art off the UI thread into raw RGBA.
pub fn decode_art(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.thumbnail(360, 480).to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

// The Android package ships the Vulkan codec on its two 64-bit ABIs only.
fn client_codecs() -> impl Iterator<Item = CodecPreference> {
    CodecPreference::ALL
        .into_iter()
        .filter(|&codec| codec != CodecPreference::PyroWave || hestia_media::video_decoder::PYROWAVE_SUPPORTED)
}

pub(crate) fn client_codec(codec: CodecPreference) -> CodecPreference {
    client_codecs()
        .find(|&available| available == codec)
        .unwrap_or_default()
}

/// The settings form for `s` on a display whose native mode is `native`.
pub fn settings_form(s: &StreamSettings, native: Option<(u32, u32, u32)>) -> SettingsForm {
    let (sizes, rates) = mode_options(native, s);
    let index = |found: Option<usize>| found.unwrap_or(0) as i32;
    SettingsForm {
        resolution_options: sizes.iter().map(|&m| resolution_label(m, native)).collect(),
        fps_options: rates.iter().map(|f| format!("{f} fps")).collect(),
        preset: index(Preset::ALL.iter().position(|&p| p == s.preset)),
        preset_hint: preset_hint(native),
        video_scale: s.video_scale.index(),
        resolution: index(sizes.iter().position(|&m| m == (s.width, s.height))),
        fps: index(rates.iter().position(|&f| f == s.fps)),
        bitrate_mbps: s.bitrate_kbps as f32 / 1000.0,
        metered_bitrate_mbps: s.metered_bitrate_kbps as f32 / 1000.0,
        codec_options: client_codecs()
            .map(|codec| match codec {
                CodecPreference::PyroWave => "PyroWave (SDR)".into(),
                _ => codec.label().into(),
            })
            .collect(),
        codec: index(client_codecs().position(|c| c == s.codec)),
        audio: index(AUDIO_LAYOUTS.iter().position(|&a| a == s.audio)),
        hdr: s.hdr,
        virtual_display: s.virtual_display,
        host_audio: s.play_audio_on_host,
        gamescope: s.launch_mode == LaunchMode::Gamescope,
        optimize: s.optimize_game_settings,
        clipboard: s.clipboard_sync,
        show_report: s.show_stream_report,
        show_menu_button: s.show_menu_button,
        hardware_decoding: s.hardware_decoding,
        gpu_video: s.gpu_video,
        direct_video: s.direct_video,
        frame_pacing: s.frame_pacing.index(),
        orientation: index(
            hestia_core::StreamOrientation::ALL
                .iter()
                .position(|&o| o == s.orientation),
        ),
    }
}

/// The settings `form` describes, over `base` (fields the form lacks).
pub fn settings_from_form(form: &SettingsForm, base: &StreamSettings) -> StreamSettings {
    let pick = |i: i32, len: usize| (i.max(0) as usize).min(len - 1);
    let chosen = |options: &[String], index: i32| {
        options
            .get(index.max(0) as usize)
            .map(|label| numbers(label))
            .unwrap_or_default()
    };
    let (width, height) = match chosen(&form.resolution_options, form.resolution)[..] {
        [w, h, ..] if w > 0 && h > 0 => (w, h),
        _ => (base.width, base.height),
    };
    let fps = match chosen(&form.fps_options, form.fps)[..] {
        [f, ..] if f > 0 => f,
        _ => base.fps,
    };
    StreamSettings {
        width,
        height,
        fps,
        bitrate_kbps: bitrate_from_mbps(form.bitrate_mbps),
        metered_bitrate_kbps: if form.metered_bitrate_mbps > 0.0 {
            bitrate_from_mbps(form.metered_bitrate_mbps)
        } else {
            0
        },
        preset: Preset::ALL[pick(form.preset, Preset::ALL.len())],
        video_scale: VIDEO_SCALES[pick(form.video_scale, VIDEO_SCALES.len())],
        codec: client_codecs().nth(form.codec.max(0) as usize).unwrap_or_default(),
        audio: AUDIO_LAYOUTS[pick(form.audio, AUDIO_LAYOUTS.len())],
        hdr: form.hdr,
        virtual_display: form.virtual_display,
        play_audio_on_host: form.host_audio,
        launch_mode: if form.gamescope {
            LaunchMode::Gamescope
        } else {
            LaunchMode::Normal
        },
        optimize_game_settings: form.optimize,
        clipboard_sync: form.clipboard,
        show_stream_report: form.show_report,
        show_menu_button: form.show_menu_button,
        hardware_decoding: form.hardware_decoding,
        gpu_video: form.gpu_video,
        direct_video: form.direct_video,
        frame_pacing: hestia_core::FramePacing::ALL[pick(form.frame_pacing, hestia_core::FramePacing::ALL.len())],
        orientation: hestia_core::StreamOrientation::ALL
            [pick(form.orientation, hestia_core::StreamOrientation::ALL.len())],
        ..base.clone()
    }
}

/// Shows `s` in the settings form, and the stream options that follow it.
pub fn load_settings_into(ui: &dyn View, s: &StreamSettings, native: Option<(u32, u32, u32)>) {
    ui.show_settings(settings_form(s, native));
    ui.set_stream_video_scale(s.video_scale.index());
    ui.set_stream_show_menu_button(s.show_menu_button);
}

/// The settings as the form shows them now, over `base`.
pub fn read_settings_from(ui: &dyn View, base: &StreamSettings) -> StreamSettings {
    settings_from_form(&ui.settings_form(), base)
}

/// "Hestia RS 0.1.0 (abc1234)"; CI sets HESTIA_BUILD_ID to the commit.
pub fn version_text() -> String {
    let build = option_env!("HESTIA_BUILD_ID")
        .filter(|id| !id.is_empty())
        .map_or_else(|| "local build".to_owned(), |id| id.chars().take(7).collect());
    format!("Hestia RS {} ({build})", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn options_include_native_and_current_modes_once() {
        let mut s = StreamSettings::default();
        (s.width, s.height, s.fps) = (2146, 964, 60);
        let (sizes, rates) = mode_options(Some((2400, 1080, 120)), &s);
        assert_eq!(
            sizes,
            vec![
                (1280, 720),
                (1920, 1080),
                (2146, 964),
                (2400, 1080),
                (2560, 1440),
                (3840, 2160)
            ]
        );
        assert_eq!(rates, vec![30, 60, 90, 120, 144]);
        assert_eq!(
            resolution_label((2400, 1080), Some((2400, 1080, 120))),
            "2400 × 1080 (native)"
        );
        assert_eq!(numbers("2400 × 1080 (native)"), vec![2400, 1080]);
        assert_eq!(numbers("120 fps"), vec![120]);
    }

    #[test]
    fn custom_resolutions() {
        assert_eq!(parse_resolution("2560x1080"), Some((2560, 1080)));
        assert_eq!(parse_resolution(" 1366 × 768 "), Some((1366, 768)));
        assert_eq!(parse_resolution("1365*767"), Some((1364, 766)), "made even");
        assert_eq!(parse_resolution("8000x1000"), None);
        assert_eq!(parse_resolution("1920"), None);
        assert_eq!(parse_resolution("1920x1080x3"), None);
        assert_eq!(parse_resolution("wide"), None);
    }

    #[test]
    fn the_settings_form_round_trips() {
        let native = Some((2400, 1080, 120));
        let mut s = StreamSettings::default();
        (s.width, s.height, s.fps, s.bitrate_kbps) = (2400, 1080, 120, 46_250);
        s.metered_bitrate_kbps = 2_500;
        s.hdr = true;
        s.gpu_video = false;
        s.direct_video = false;
        s.frame_pacing = hestia_core::FramePacing::MaxSmoothness;
        s.orientation = hestia_core::StreamOrientation::Portrait;
        s.launch_mode = LaunchMode::Gamescope;
        let form = settings_form(&s, native);
        assert_eq!(
            form.resolution_options[form.resolution as usize],
            "2400 × 1080 (native)"
        );
        assert_eq!(form.fps_options[form.fps as usize], "120 fps");
        assert_eq!(settings_from_form(&form, &StreamSettings::default()), s);
        // Out-of-range choices keep the base's mode.
        let broken = SettingsForm {
            resolution: 99,
            fps: -1,
            ..form
        };
        let back = settings_from_form(&broken, &s);
        assert_eq!((back.width, back.height), (s.width, s.height));
        assert_eq!(back.fps, 30, "a negative index picks the first rate");
    }

    #[test]
    fn bitrate_keeps_quarter_megabits() {
        assert_eq!(bitrate_from_mbps(46.25), 46_250);
        assert_eq!(bitrate_from_mbps(3.5), 3_500);
        assert_eq!(bitrate_from_mbps(23.61), 23_500);
        assert_eq!(bitrate_from_mbps(0.0), 500);
    }
}
