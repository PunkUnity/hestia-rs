//! Media backends that consume a [`hestia_stream`] session.
//!
//! Each backend implements [`VideoSink`] / [`AudioSink`], so the streaming
//! code never knows which decoder or audio API is in use.
//!
//! Available today:
//! * [`decode`]: [`DecodingSink`], which decodes on its own thread with any
//!   [`FrameDecoder`] and hands decoded frames to a [`FramePresenter`].
//! * `software_h264` (feature `openh264`, on by default): OpenH264 software
//!   H.264 decoding, on every platform. On the desktop it hands 4:2:0 planes
//!   to the window's GL renderer, which converts them to RGB on the GPU
//!   ([`native_picture`]), and converts on the CPU ([`color`]) otherwise.
//! * [`audio_playback`]: independently selected Opus decoders and PCM outputs,
//!   connected by the low-latency playback FIFO.
//! * [`null`]: counts frames and packets; for exercising the pipeline.
//! * [`dump`]: writes the elementary video stream to a file (`ffplay out.h264`).
//!
//! * `mediacodec` (Android): hardware H.264/HEVC/AV1 through `AMediaCodec`,
//!   rendered into GPU buffers the UI draws directly (`gpu_video`), or read
//!   back on the CPU; falls back to OpenH264 for H.264.
//! * `vaapi` (Linux, feature `vaapi`): hardware H.264/HEVC/AV1 through
//!   VA-API, its NV12 pictures shared with the window's renderer as
//!   DMA-BUFs, or read back.
//!
//! See `docs/VIDEO.md` in the repository for the whole video pipeline.

#[cfg(target_os = "android")]
pub mod aaudio;
#[cfg(all(feature = "audio", target_os = "linux"))]
mod alsa_surround;
#[cfg(target_os = "android")]
pub mod android_audio;
pub mod audio_fifo;
#[cfg(feature = "audio")]
pub mod audio_out;
pub mod audio_playback;
pub mod color;
pub mod decode;
#[cfg(target_os = "android")]
pub mod direct_video;
#[cfg(any(target_os = "android", test))]
mod direct_video_stats;
pub mod dump;
#[cfg(target_os = "android")]
pub mod gpu_video;
#[cfg(target_os = "android")]
pub mod mediacodec;
pub mod native_picture;
pub mod null;
#[cfg(feature = "audio")]
pub mod opus_decode;
pub mod pacing;
pub mod picture_mailbox;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod pyrowave;
#[cfg(feature = "openh264")]
pub mod software_h264;
#[cfg(all(feature = "vaapi", target_os = "linux"))]
pub mod vaapi;
pub mod video_decoder;

pub use color::Colorimetry;
pub use decode::{
    DecodeStats, DecodedFrame, DecodingSink, FrameBuffer, FrameDecoder, FramePresenter, LatencyHistogram,
    RecoveryLatency,
};
pub use hestia_stream::sink::{AudioFormat, AudioSink, EncodedFrame, SubmitResult, VideoCodec, VideoFormat, VideoSink};

pub use video_decoder::{DecoderChoice, decoder_with, default_decoder};

#[cfg(target_os = "android")]
pub use video_decoder::decoder_with_android_capabilities;

/// Description of a decoder backend, for settings screens and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    pub name: &'static str,
    pub hardware: bool,
    pub available: bool,
}

/// Backends compiled into this build, best first.
pub fn video_backends() -> Vec<BackendInfo> {
    vec![
        BackendInfo {
            name: if cfg!(target_os = "android") {
                "MediaCodec"
            } else {
                "VA-API"
            },
            hardware: true,
            available: cfg!(any(target_os = "android", all(feature = "vaapi", target_os = "linux"))),
        },
        BackendInfo {
            name: "OpenH264 (software H.264)",
            hardware: false,
            available: cfg!(feature = "openh264"),
        },
        BackendInfo {
            name: "Null (testing)",
            hardware: false,
            available: true,
        },
    ]
}

/// Audio playback when this build has it, otherwise a sink that only counts.
pub fn default_audio_sink() -> Box<dyn AudioSink> {
    #[cfg(target_os = "android")]
    {
        Box::new(audio_playback::PlaybackSink::new(
            android_audio::MediaCodecOpusDecoder::default(),
            aaudio::AAudioOutput::default(),
        ))
    }
    #[cfg(all(feature = "audio", not(target_os = "android")))]
    {
        Box::new(audio_playback::PlaybackSink::new(
            opus_decode::LibOpusDecoder::default(),
            audio_out::CpalOutput::default(),
        ))
    }
    #[cfg(not(any(feature = "audio", target_os = "android")))]
    {
        Box::new(null::NullAudioSink::default())
    }
}
