//! SDP in both directions: what the host offers in its DESCRIBE reply and
//! the stream parameters we ANNOUNCE (port of moonlight-common-c's
//! `SdpGenerator.c` for Sunshine-family hosts, GFE 7.1.431+ protocol).

use crate::sink::{AudioFormat, VideoCodec, VideoFormat};

/// Sunshine encryption feature bits.
pub const SS_ENC_CONTROL_V2: u32 = 0x01;
pub const SS_ENC_VIDEO: u32 = 0x02;
pub const SS_ENC_AUDIO: u32 = 0x04;

// Host feature bits (`x-ss-general.featureFlags`).

/// The host takes pen and touch events (`InputEvent::Pen`, `InputEvent::Touch`).
pub const SS_FF_PEN_TOUCH_EVENTS: u32 = 0x01;
/// The host takes controller touchpad and motion events.
pub const SS_FF_CONTROLLER_TOUCH_EVENTS: u32 = 0x02;

/// Moonlight client feature flags we advertise.
const ML_FF_FEC_STATUS: u32 = 0x01;
const ML_FF_SESSION_ID_V1: u32 = 0x02;

const NVFF_BASE: u32 = 0x07;
const NVFF_AUDIO_ENCRYPTION: u32 = 0x20;
const NVFF_RI_ENCRYPTION: u32 = 0x80;

/// Size of the per-packet video encryption prefix.
pub const ENC_VIDEO_HEADER_LEN: u32 = 12 + 4 + 16;

/// The host's DESCRIBE payload.
#[derive(Debug, Clone, Default)]
pub struct HostSdp {
    pub raw: String,
}

fn uint_attribute(raw: &str, name: &str) -> Option<u32> {
    let at = raw.find(name)?;
    let rest = &raw[at + name.len()..];
    let colon = rest.find(':')?;
    let value: String = rest[colon + 1..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == 'x' || *c == 'X')
        .collect();
    if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        value
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    }
}

impl HostSdp {
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    pub fn supports_hevc(&self) -> bool {
        self.raw.contains("sprop-parameter-sets=AAAAAU")
    }

    pub fn supports_av1(&self) -> bool {
        self.raw.contains("AV1/90000")
    }

    pub fn supports_rfi(&self) -> bool {
        self.raw.lines().any(|line| {
            line.trim()
                .strip_prefix("a=x-nv-video[0].refPicInvalidation:")
                .is_some_and(|value| value.trim() == "1")
        })
    }

    pub fn feature_flags(&self) -> u32 {
        uint_attribute(&self.raw, "x-ss-general.featureFlags").unwrap_or(0)
    }

    pub fn encryption_supported(&self) -> u32 {
        uint_attribute(&self.raw, "x-ss-general.encryptionSupported").unwrap_or(0)
    }

    pub fn encryption_requested(&self) -> u32 {
        uint_attribute(&self.raw, "x-ss-general.encryptionRequested").unwrap_or(0)
    }

    /// Opus layouts the host offers for `channels`: (normal quality, high quality).
    pub fn opus_configs(&self, channels: u8) -> (Option<AudioFormat>, Option<AudioFormat>) {
        if channels == 2 {
            return (Some(AudioFormat::stereo()), None);
        }
        if !matches!(channels, 6 | 8) {
            return (None, None);
        }
        let prefix = format!("a=fmtp:97 surround-params={channels}");
        let mut found = self
            .raw
            .match_indices(&prefix)
            .map(|(i, _)| &self.raw[i + prefix.len()..]);
        let parse = |s: &str| -> Option<AudioFormat> {
            let digits: Vec<u8> = s.bytes().take_while(u8::is_ascii_digit).map(|b| b - b'0').collect();
            if digits.len() < 2 + channels as usize {
                return None;
            }
            let mut mapping = [0u8; 8];
            mapping[..channels as usize].copy_from_slice(&digits[2..2 + channels as usize]);
            Some(AudioFormat {
                sample_rate: 48_000,
                channels,
                streams: digits[0],
                coupled_streams: digits[1],
                samples_per_frame: 240,
                mapping,
            })
        };
        let normal = found.next().and_then(parse);
        let high = found.next().and_then(parse);
        let normal = normal.map(|mut cfg| {
            // Some Sunshine-family hosts rotate only the first six positions
            // of their 7.1 map. Their high-quality line carries the same
            // speaker map in the correct order, with different stream counts.
            if let Some(high) = high
                && (channels, cfg.streams, cfg.coupled_streams) == (8, 5, 3)
                && (high.streams, high.coupled_streams) == (8, 0)
            {
                let mut truncated = high.mapping;
                truncated[3..6].rotate_left(1);
                if cfg.mapping == truncated {
                    cfg.mapping = high.mapping;
                    return cfg;
                }
            }
            // GFE announces normal quality with LFE last. Restore
            // GameStream playback order (FL FR FC LFE BL BR SL SR).
            cfg.mapping[3..usize::from(channels)].rotate_right(1);
            cfg
        });
        (normal, high)
    }

    /// Picks the first offered format supported by the host.
    pub fn negotiate_video(&self, wanted: &[VideoFormat], server_codec_modes: u32) -> Option<VideoFormat> {
        use hestia_codec_flags::*;

        wanted.iter().copied().find(|format| {
            let advertised = match format.codec {
                VideoCodec::H264 => true, // Legacy GameStream hosts always support baseline H.264.
                VideoCodec::Hevc => self.supports_hevc(),
                VideoCodec::Av1 => self.supports_av1(),
                VideoCodec::PyroWave => self
                    .raw
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("a=rtpmap:"))
                    .any(|mapping| mapping.split_whitespace().nth(1) == Some("PYROWAVE/90000")),
            };
            let required_flag = match (format.codec, format.ten_bit, format.yuv444) {
                (VideoCodec::PyroWave, false, false) => SCM_PYROWAVE,
                (_, false, false) => return advertised,
                (VideoCodec::H264, false, true) => SCM_H264_HIGH8_444,
                (VideoCodec::H264, true, _) | (VideoCodec::PyroWave, _, _) => return false,
                (VideoCodec::Hevc, true, true) => SCM_HEVC_REXT10_444,
                (VideoCodec::Hevc, true, false) => SCM_HEVC_MAIN10,
                (VideoCodec::Hevc, false, true) => SCM_HEVC_REXT8_444,
                (VideoCodec::Av1, true, true) => SCM_AV1_HIGH10_444,
                (VideoCodec::Av1, true, false) => SCM_AV1_MAIN10,
                (VideoCodec::Av1, false, true) => SCM_AV1_HIGH8_444,
            };
            advertised && server_codec_modes & required_flag != 0
        })
    }
}

/// Server codec mode flags, duplicated here to keep this crate standalone.
mod hestia_codec_flags {
    pub const SCM_PYROWAVE: u32 = 0x4000_0000;
    pub const SCM_HEVC_MAIN10: u32 = 0x0000_0200;
    pub const SCM_AV1_MAIN10: u32 = 0x0002_0000;
    pub const SCM_H264_HIGH8_444: u32 = 0x0004_0000;
    pub const SCM_HEVC_REXT8_444: u32 = 0x0008_0000;
    pub const SCM_HEVC_REXT10_444: u32 = 0x0010_0000;
    pub const SCM_AV1_HIGH8_444: u32 = 0x0020_0000;
    pub const SCM_AV1_HIGH10_444: u32 = 0x0040_0000;
}

/// Everything that goes into our ANNOUNCE.
#[derive(Debug, Clone)]
pub struct AnnounceParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Max video payload per packet (already a multiple of 16).
    pub packet_size: u32,
    pub bitrate_kbps: u32,
    pub format: VideoFormat,
    pub audio_channels: u8,
    pub audio_channel_mask: u32,
    pub high_quality_audio: bool,
    pub audio_packet_duration_ms: u32,
    pub encryption_enabled: u32,
    pub audio_encryption: bool,
    pub streaming_remotely: bool,
    pub ipv6: bool,
    /// Address as written in the `o=` line.
    pub host_address: String,
    pub video_port: u16,
    pub refresh_rate_x100: u32,
    /// `(colorSpace << 1) | colorRange`; 0 = Rec. 601 limited range.
    pub encoder_csc_mode: u32,
}

/// Builds the ANNOUNCE body. Attribute names and order follow common-c.
pub fn build_announce(p: &AnnounceParams) -> String {
    build_announce_with_reference_recovery(p, false)
}

pub(crate) fn build_announce_with_reference_recovery(p: &AnnounceParams, reference_frame_invalidation: bool) -> String {
    let mut attrs: Vec<(&str, String)> = Vec::new();
    let mut a = |name: &'static str, value: String| attrs.push((name, value));

    a(
        "x-ml-general.featureFlags",
        (ML_FF_FEC_STATUS | ML_FF_SESSION_ID_V1).to_string(),
    );
    a("x-ss-general.encryptionEnabled", p.encryption_enabled.to_string());
    a(
        "x-ss-video[0].chromaSamplingType",
        u8::from(p.format.yuv444).to_string(),
    );

    a("x-nv-video[0].clientViewportWd", p.width.to_string());
    a("x-nv-video[0].clientViewportHt", p.height.to_string());
    a("x-nv-video[0].maxFPS", p.fps.to_string());
    let packet_size = if p.encryption_enabled & SS_ENC_VIDEO != 0 {
        p.packet_size - ENC_VIDEO_HEADER_LEN
    } else {
        p.packet_size
    };
    a("x-nv-video[0].packetSize", packet_size.to_string());
    a("x-nv-video[0].rateControlMode", "4".into());
    a("x-nv-video[0].timeoutLengthMs", "7000".into());
    a("x-nv-video[0].framesWithInvalidRefThreshold", "0".into());

    // 20% of the requested bitrate is left for FEC.
    let mut adjusted = (p.bitrate_kbps as u64 * 80 / 100) as u32;
    if p.streaming_remotely && adjusted > 500 {
        adjusted -= 500;
    }
    let adjusted = adjusted.min(100_000).to_string();
    a("x-nv-video[0].initialBitrateKbps", adjusted.clone());
    a("x-nv-video[0].initialPeakBitrateKbps", adjusted.clone());
    a("x-nv-vqos[0].bw.minimumBitrateKbps", adjusted.clone());
    a("x-nv-vqos[0].bw.maximumBitrateKbps", adjusted);
    a("x-ml-video.configuredBitrateKbps", p.bitrate_kbps.to_string());

    a("x-nv-vqos[0].fec.enable", "1".into());
    a("x-nv-vqos[0].videoQualityScoreUpdateTime", "5000".into());
    let (vqos, aqos) = if p.streaming_remotely { ("0", "0") } else { ("5", "4") };
    a("x-nv-vqos[0].qosTrafficType", vqos.into());
    a("x-nv-aqos.qosTrafficType", aqos.into());

    // Gen 5+ (7.1.431+) options
    let mut feature_flags = NVFF_BASE | NVFF_RI_ENCRYPTION;
    if p.audio_encryption {
        feature_flags |= NVFF_AUDIO_ENCRYPTION;
    }
    a("x-nv-general.featureFlags", feature_flags.to_string());
    a("x-nv-general.useReliableUdp", "13".into());
    a("x-nv-vqos[0].fec.minRequiredFecPackets", "2".into());
    a("x-nv-vqos[0].bllFec.enable", "0".into());
    if p.width < 720 || p.height < 540 {
        a("x-nv-vqos[0].drc.enable", "1".into());
        a("x-nv-vqos[0].drc.tableType", "2".into());
    } else {
        a("x-nv-vqos[0].drc.enable", "0".into());
    }
    a("x-nv-general.enableRecoveryMode", "0".into());

    a("x-nv-video[0].videoEncoderSlicesPerFrame", "1".into());
    match p.format.codec {
        VideoCodec::PyroWave => a("x-nv-vqos[0].bitStreamFormat", "64".into()),
        VideoCodec::Av1 => a("x-nv-vqos[0].bitStreamFormat", "2".into()),
        VideoCodec::Hevc => {
            a("x-nv-clientSupportHevc", "1".into());
            a("x-nv-vqos[0].bitStreamFormat", "1".into());
        }
        VideoCodec::H264 => {
            a("x-nv-clientSupportHevc", "0".into());
            a("x-nv-vqos[0].bitStreamFormat", "0".into());
        }
    }
    a("x-nv-video[0].dynamicRangeMode", u8::from(p.format.ten_bit).to_string());
    // Without reference frame invalidation, keep a single reference frame so
    // a lost frame only costs us until the next IDR.
    let max_refs = if reference_frame_invalidation { "0" } else { "1" };
    a("x-nv-video[0].maxNumReferenceFrames", max_refs.into());
    a("x-nv-video[0].clientRefreshRateX100", p.refresh_rate_x100.to_string());

    a("x-nv-audio.surround.numChannels", p.audio_channels.to_string());
    a("x-nv-audio.surround.channelMask", p.audio_channel_mask.to_string());
    a(
        "x-nv-audio.surround.enable",
        if p.audio_channels > 2 { "1" } else { "0" }.into(),
    );
    a(
        "x-nv-audio.surround.AudioQuality",
        if p.high_quality_audio { "1" } else { "0" }.into(),
    );
    a("x-nv-aqos.packetDuration", p.audio_packet_duration_ms.to_string());
    a("x-nv-video[0].encoderCscMode", p.encoder_csc_mode.to_string());

    let mut out = format!(
        "v=0\r\no=android 0 14 IN {} {}\r\ns=NVIDIA Streaming Client\r\n",
        if p.ipv6 { "IPv6" } else { "IPv4" },
        p.host_address
    );
    for (name, value) in attrs {
        out.push_str("a=");
        out.push_str(name);
        out.push(':');
        out.push_str(&value);
        out.push_str(" \r\n");
    }
    out.push_str(&format!("t=0 0\r\nm=video {}  \r\n", p.video_port));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERMES_DESCRIBE: &str = "a=x-ss-general.featureFlags:3\n\
a=x-ss-general.encryptionSupported:7\n\
a=x-ss-general.encryptionRequested:1\n\
sprop-parameter-sets=AAAAAU\n\
a=rtpmap:98 AV1/90000\n\
a=fmtp:97 surround-params=21101\n\
a=fmtp:97 surround-params=642012345\n\
a=fmtp:97 surround-params=660012345\n\
a=fmtp:97 surround-params=85301234567\n\
a=fmtp:97 surround-params=88001234567\n";

    #[test]
    fn parses_describe() {
        let sdp = HostSdp::new(HERMES_DESCRIBE);
        assert!(sdp.supports_hevc() && sdp.supports_av1() && !sdp.supports_rfi());
        assert_eq!(sdp.feature_flags(), 3);
        assert_eq!(sdp.encryption_supported(), 7);
        assert_eq!(sdp.encryption_requested(), SS_ENC_CONTROL_V2);
        let (normal, high) = sdp.opus_configs(6);
        let normal = normal.unwrap();
        assert_eq!((normal.streams, normal.coupled_streams), (4, 2));
        // LFE (last digit) moved after centre.
        assert_eq!(&normal.mapping[..6], &[0, 1, 2, 5, 3, 4]);
        assert_eq!(high.unwrap().streams, 6);
        assert_eq!(sdp.opus_configs(2).0.unwrap(), AudioFormat::stereo());
    }

    #[test]
    fn parses_7_1_from_current_and_older_sunshine_family_hosts() {
        for normal in ["85301245673", "85301245367"] {
            let sdp = HostSdp::new(format!(
                "a=fmtp:97 surround-params={normal}\r\na=fmtp:97 surround-params=88001234567\r\n"
            ));
            let (normal, high) = sdp.opus_configs(8);
            let normal = normal.unwrap();
            let high = high.unwrap();
            assert_eq!((normal.streams, normal.coupled_streams), (5, 3));
            assert_eq!(normal.mapping, high.mapping);
            assert_eq!(normal.mapping, [0, 1, 2, 3, 4, 5, 6, 7]);
        }
    }

    #[test]
    fn negotiates_codecs() {
        let sdp = HostSdp::new(format!("{HERMES_DESCRIBE}\na=rtpmap:98 AV1/90000\n"));
        let h264 = VideoFormat {
            codec: VideoCodec::H264,
            ten_bit: false,
            yuv444: false,
        };
        let hevc = VideoFormat {
            codec: VideoCodec::Hevc,
            ten_bit: false,
            yuv444: false,
        };
        let av1 = VideoFormat {
            codec: VideoCodec::Av1,
            ..h264
        };
        let hevc_main10 = VideoFormat { ten_bit: true, ..hevc };
        assert_eq!(sdp.negotiate_video(&[h264, hevc, av1], u32::MAX), Some(h264));
        assert_eq!(sdp.negotiate_video(&[hevc, h264, av1], u32::MAX), Some(hevc));
        assert_eq!(sdp.negotiate_video(&[av1, h264], u32::MAX), Some(av1));
        assert_eq!(sdp.negotiate_video(&[hevc_main10, hevc], 0), Some(hevc));
        assert_eq!(sdp.negotiate_video(&[hevc_main10, hevc], u32::MAX), Some(hevc_main10));
        let plain = HostSdp::new("a=x-ss-general.featureFlags:0\n");
        assert_eq!(plain.negotiate_video(&[hevc, h264], u32::MAX), Some(h264));
        assert_eq!(plain.negotiate_video(&[hevc, av1], u32::MAX), None);
    }

    #[test]
    fn pyrowave_requires_both_host_advertisements_and_sdr_420() {
        let pyro = VideoFormat {
            codec: VideoCodec::PyroWave,
            ten_bit: false,
            yuv444: false,
        };
        let host = HostSdp::new("a=rtpmap:98 PYROWAVE/90000\r\n");
        let mask = hestia_codec_flags::SCM_PYROWAVE;
        assert_eq!(host.negotiate_video(&[pyro], mask), Some(pyro));
        assert_eq!(HostSdp::default().negotiate_video(&[pyro], mask), None);
        assert_eq!(host.negotiate_video(&[pyro], 0), None);
        assert_eq!(
            host.negotiate_video(
                &[
                    VideoFormat { ten_bit: true, ..pyro },
                    VideoFormat { yuv444: true, ..pyro }
                ],
                mask
            ),
            None
        );
    }

    #[test]
    fn reference_invalidation_requires_the_enabled_sdp_attribute() {
        for sdp in [
            "",
            "a=x-nv-video[0].refPicInvalidation:0\r\n",
            "a=x-nv-video[0].refPicInvalidation\r\na=other:1\r\n",
        ] {
            assert!(!HostSdp::new(sdp).supports_rfi());
        }
        assert!(HostSdp::new("a=x-nv-video[0].refPicInvalidation:1\r\n").supports_rfi());
    }

    #[test]
    fn announce_has_everything_hermes_requires() {
        let p = AnnounceParams {
            width: 1920,
            height: 1080,
            fps: 60,
            packet_size: 1392,
            bitrate_kbps: 20_000,
            format: VideoFormat {
                codec: VideoCodec::H264,
                ten_bit: false,
                yuv444: false,
            },
            audio_channels: 2,
            audio_channel_mask: 3,
            high_quality_audio: false,
            audio_packet_duration_ms: 5,
            encryption_enabled: SS_ENC_CONTROL_V2,
            audio_encryption: false,
            streaming_remotely: false,
            ipv6: false,
            host_address: "192.168.0.10".into(),
            video_port: 47998,
            refresh_rate_x100: 6000,
            encoder_csc_mode: 0,
        };
        let sdp = build_announce(&p);
        assert!(
            build_announce_with_reference_recovery(&p, true).contains("a=x-nv-video[0].maxNumReferenceFrames:0 \r\n")
        );
        let mut pyro_params = p;
        pyro_params.format.codec = VideoCodec::PyroWave;
        let pyro = build_announce(&pyro_params);
        assert!(
            pyro.lines()
                .any(|line| line.trim() == "a=x-nv-vqos[0].bitStreamFormat:64")
        );

        assert!(sdp.starts_with("v=0\r\no=android 0 14 IN IPv4 192.168.0.10\r\ns=NVIDIA Streaming Client\r\n"));
        assert!(sdp.ends_with("t=0 0\r\nm=video 47998  \r\n"));
        // Every key Hermes' cmd_announce reads with args.at(...)
        for key in [
            "x-nv-audio.surround.numChannels",
            "x-nv-audio.surround.channelMask",
            "x-nv-aqos.packetDuration",
            "x-nv-audio.surround.AudioQuality",
            "x-nv-general.useReliableUdp",
            "x-nv-video[0].packetSize",
            "x-nv-vqos[0].fec.minRequiredFecPackets",
            "x-ml-general.featureFlags",
            "x-nv-aqos.qosTrafficType",
            "x-nv-vqos[0].qosTrafficType",
            "x-ss-general.encryptionEnabled",
            "x-nv-general.featureFlags",
            "x-nv-video[0].clientViewportHt",
            "x-nv-video[0].clientViewportWd",
            "x-nv-video[0].maxFPS",
            "x-nv-vqos[0].bw.maximumBitrateKbps",
            "x-nv-video[0].videoEncoderSlicesPerFrame",
            "x-nv-video[0].maxNumReferenceFrames",
            "x-nv-video[0].encoderCscMode",
            "x-nv-vqos[0].bitStreamFormat",
            "x-nv-video[0].dynamicRangeMode",
            "x-ss-video[0].chromaSamplingType",
            "x-ml-video.configuredBitrateKbps",
        ] {
            assert!(sdp.contains(&format!("a={key}:")), "missing {key}");
        }
        assert!(sdp.contains("a=x-nv-vqos[0].bw.maximumBitrateKbps:16000 \r\n"));
        assert!(sdp.contains("a=x-nv-general.featureFlags:135 \r\n"));
        assert!(sdp.contains("a=x-nv-video[0].maxNumReferenceFrames:1 \r\n"));
    }
}
