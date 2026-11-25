//! The streaming data plane, in pure Rust.
//!
//! Once `/launch` succeeds, a GameStream client:
//!
//! 1. **RTSP handshake** ([`rtsp`], TCP 48010, AES-GCM framed when
//!    `rtspenc://`): OPTIONS, DESCRIBE (host SDP), SETUP for audio, video and
//!    control (ports, session id, ping payload, connect data), ANNOUNCE (our
//!    SDP, [`sdp`]) and PLAY.
//! 2. **Control stream** ([`control`] over [`enet`], UDP 47999, AES-GCM):
//!    start packets, periodic pings, IDR requests, input; termination, rumble
//!    and HDR state from the host.
//! 3. **Video** ([`video`], RTP over UDP 47998): pings to open the path,
//!    FEC-block reassembly with Reed–Solomon recovery ([`fec`]), frame
//!    depacketization.
//! 4. **Audio** ([`audio`], RTP over UDP 48000): Opus packets, optional AES-CBC.
//! 5. **Input** ([`input`]): events encoded into input packets sent on the
//!    control stream.
//!
//! [`StreamSession`] runs all of it; decoders plug in through the
//! [`VideoSink`] / [`AudioSink`] traits.

pub mod audio;
pub mod control;
pub mod enet;
pub mod fec;
pub mod input;
#[cfg(test)]
mod mock_host;
pub mod rtsp;
pub mod sdp;
pub mod session;
pub mod sink;
pub mod video;

use std::fmt;

pub use input::InputEvent;
pub use rtsp::SessionUrl;
pub use session::StreamSession;
pub use sink::{
    AudioFormat, AudioSink, EncodedFrame, KeyframeRequester, SubmitResult, VideoCodec, VideoFormat, VideoSink,
};

/// Everything needed to open the stream, taken from the launch request and
/// the host's `/launch` answer.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub session_url: SessionUrl,
    /// `ServerCodecModeSupport` from `/serverinfo`.
    pub server_codec_modes: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Max UDP payload for video packets (rounded down to a multiple of 16).
    pub packet_size: u32,
    /// Formats the decoder handles, best first.
    pub video_formats: Vec<VideoFormat>,
    pub audio_channels: u8,
    pub audio_channel_mask: u32,
    /// `rikey` / `rikeyid` from `/launch`.
    pub remote_input_key: [u8; 16],
    pub remote_input_key_id: u32,
    pub streaming_remotely: bool,
    /// Experimental: keep decoding delta frames after a lost frame while
    /// the keyframe is on its way (artifacts) instead of freezing.
    pub show_video_through_loss: bool,
}

impl StreamConfig {
    /// Typical LAN packet size (Moonlight uses 1392 locally, 1024 remotely).
    pub const LAN_PACKET_SIZE: u32 = 1392;
    pub const WAN_PACKET_SIZE: u32 = 1024;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStage {
    Rtsp,
    Control,
    Video,
    Audio,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamStats {
    pub frames: u64,
    pub packets: u64,
    pub bytes: u64,
    pub recovered_packets: u64,
    pub lost_frames: u64,
    pub dropped_waiting_idr: u64,
    pub dropped_waiting_rfi: u64,
    pub idr_requests: u64,
    pub rtt_ms: u32,
    pub rtt_variance_ms: u32,
    /// Running totals of host-reported frame processing latency.
    pub host_latency_us: u64,
    pub host_latency_frames: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StageStarting(StreamStage),
    StageFailed(StreamStage, String),
    Connected,
    FirstFrame,
    /// Roughly once a second.
    Stats(StreamStats),
    Rumble {
        gamepad: u8,
        low: u16,
        high: u16,
    },
    HdrMode(bool),
    Terminated {
        reason: String,
        graceful: bool,
    },
}

pub trait SessionListener: Send + Sync {
    fn on_event(&self, event: StreamEvent);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    Rtsp(String),
    Io(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rtsp(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StreamError {}

/// True for RFC 1918 / ULA / link-local addresses (stream "locally").
pub fn is_private_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            v6.is_loopback() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
    }
}
