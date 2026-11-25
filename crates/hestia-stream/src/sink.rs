//! What a streaming session produces and where it goes. Decoders and renderers
//! (in `hestia-media` or platform code) implement these traits, which keeps
//! the network stack independent of any codec or graphics API.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    PyroWave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    pub codec: VideoCodec,
    pub ten_bit: bool,
    pub yuv444: bool,
}

/// One complete access unit, reassembled and FEC-recovered.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub frame_number: u32,
    pub keyframe: bool,
    /// Annex B bitstream (H.264/HEVC) or OBU stream (AV1), or a complete opaque PyroWave packet.
    pub data: Vec<u8>,
    /// When the first packet of this frame arrived.
    pub received_at: Instant,
    /// First request in the last recovery that supplied a complete keyframe.
    pub idr_requested_at: Option<Instant>,
    pub loss_detected_at: Option<Instant>,
    /// Host-side capture→send latency, when the host reports it.
    pub host_latency: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitResult {
    Ok,
    /// The decoder lost its reference; ask the host for an IDR frame.
    NeedKeyframe,
}

/// Lets a sink that decodes on its own thread ask the host for an IDR frame
/// after the fact (a decode error a few frames after `submit` returned).
#[derive(Clone)]
pub struct KeyframeRequester(Arc<dyn Fn() + Send + Sync>);

impl KeyframeRequester {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn request(&self) {
        (self.0)()
    }
}

impl fmt::Debug for KeyframeRequester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyframeRequester")
    }
}

/// `submit` runs on the network receive thread: implementations must not
/// block on decoding there (queue the frame and decode elsewhere).
pub trait VideoSink: Send {
    /// Whether the decoder can resume from a host-marked reference recovery frame.
    fn supports_reference_frame_invalidation(&self, _format: VideoFormat) -> bool {
        false
    }

    fn setup(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String>;
    /// Called once before `setup` with a handle for asynchronous IDR requests.
    fn attach_keyframe_requester(&mut self, _requester: KeyframeRequester) {}
    /// Reports a permanent sink failure from the decoder thread.
    fn attach_failure_handler(&mut self, _handler: Box<dyn Fn(String) + Send>) {}
    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult;
    fn cleanup(&mut self) {}
}

/// Opus multistream layout announced by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub streams: u8,
    pub coupled_streams: u8,
    pub samples_per_frame: u32,
    /// Opus indices in playback order: FL FR FC LFE BL BR SL SR.
    pub mapping: [u8; 8],
}

impl AudioFormat {
    pub fn stereo() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            streams: 1,
            coupled_streams: 1,
            samples_per_frame: 240,
            mapping: [0, 1, 0, 0, 0, 0, 0, 0],
        }
    }
}

pub trait AudioSink: Send {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String>;
    /// One Opus packet (already decrypted). An empty slice marks a lost
    /// packet so the decoder can run packet-loss concealment.
    fn submit(&mut self, opus_packet: &[u8]);
    fn cleanup(&mut self) {}
}
