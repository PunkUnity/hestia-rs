//! Android hardware decoding with `AMediaCodec`.
//!
//! The GPU path samples MediaCodec output directly. If a mode is unavailable
//! or fails during playback, try SDR pictures from the image reader, then
//! the codec's own planar/semi-planar buffers, then OpenH264 for H.264.
//! Low-latency keys keep supported codec components from holding frames back.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use hestia_stream::sink::VideoCodec;
use log::{info, warn};
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection, OutputBuffer,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ndk::hardware_buffer::HardwareBufferUsage;

use crate::color::{Colorimetry, Yuv420, yuv420_to_rgba};
use crate::decode::{DecodeStats, DecodedFrame, FrameBuffer, FrameDecoder, FrameTiming};
use crate::gpu_video::{GpuOutput, GpuPicture, SharedReader};
use crate::native_picture::NativePicture;
use crate::pacing::{Pacing, Schedule, VsyncClock};
use crate::{EncodedFrame, VideoFormat};

/// Bound time spent feeding one frame when the codec is under pressure.
///
/// After a network stall, frames arrive in a burst and the codec can stay
/// busy for longer than one frame. Giving up throws away a good frame, and
/// the reference chain with it, so the stream waits for a keyframe, which
/// takes a round trip or more on a slow link. Wait instead, while releasing
/// output, and give up only when the codec looks stuck (well under
/// `OUTPUT_STALL`, which switches outputs).
const INPUT_WAIT: Duration = Duration::from_millis(500);
const INPUT_RETRY: Duration = Duration::from_millis(4);
const OUTPUT_POLL: Duration = Duration::from_millis(2);
const SLOW_OUTPUT_POLL: Duration = Duration::from_millis(8);
const FAST_POLL_WINDOW: Duration = Duration::from_millis(50);
const MAX_PENDING_FRAMES: usize = 32;
/// Try another output after repeated failures, including mid-stream.
const FAILURES_BEFORE_FALLBACK: u32 = 3;
const OUTPUT_STALL: Duration = Duration::from_secs(1);
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
/// How long the decoder waits for the direct video surface at start.
const DIRECT_SURFACE_WAIT: Duration = Duration::from_millis(1500);

// MediaCodecInfo.CodecCapabilities colour formats.
const COLOR_YUV420_PLANAR: i32 = 19;
const COLOR_YUV420_PACKED_PLANAR: i32 = 20;
const COLOR_YUV420_SEMI_PLANAR: i32 = 21;
const COLOR_YUV420_PACKED_SEMI_PLANAR: i32 = 39;
const COLOR_YUV420_FLEXIBLE: i32 = 0x7F42_0888;
/// Qualcomm's NV12 with 128-byte rows and 32-row planes.
const COLOR_QCOM_YUV420_SEMI_PLANAR_32M: i32 = 0x7FA3_0C04;

/// Where the codec puts its pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// The direct video surface behind the window: the system shows the
    /// pictures, the UI draws nothing (see `direct_video`).
    Direct,
    /// GPU buffers the UI draws directly (zero-copy).
    Gpu,
    /// An image reader's surface, read on the CPU.
    Images,
    /// The codec's own output buffers.
    Buffers,
}

enum DirectSurface {
    Ready,
    Hidden,
    Replaced,
}

fn mime(codec: VideoCodec) -> Option<&'static str> {
    match codec {
        VideoCodec::H264 => Some("video/avc"),
        VideoCodec::Hevc => Some("video/hevc"),
        VideoCodec::Av1 => Some("video/av01"),
        VideoCodec::PyroWave => None,
    }
}

/// The codec and the image reader it renders into. Neither is `Send` in
/// the ndk crate, but both may be used from any thread one at a time, and
/// only the decoder thread uses them after setup.
struct Running {
    codec: MediaCodec,
    output: Output,
    /// The image reader and its surface (kept referenced while the codec
    /// renders to it); none with buffer output. Shared with the GPU
    /// pictures still out, which must not outlive it.
    images: Option<(Arc<SharedReader>, NativeWindow)>,
    /// Buffer output's picture layout, from the codec's output format.
    layout: Option<BufferLayout>,
    width: i32,
    height: i32,
    pts_us: u64,
    frame_us: u64,
    pending: VecDeque<(u64, FrameTiming)>,
    /// Output in release order, awaiting an image from the reader.
    rendered: VecDeque<(u64, FrameTiming)>,
    /// Direct output: the surface rendered to (None while it's gone) and
    /// its generation, to follow it when the helper replaces it.
    direct: Option<(u64, Option<NativeWindow>)>,
    direct_measurement: Option<crate::direct_video::Measurement>,
    /// Direct output: how frames are paced to the display.
    pacing: Pacing,
    schedule: Schedule,
    /// Balanced pacing's vsync times.
    vsync: Option<VsyncClock>,
}

// SAFETY: see the type's documentation.
unsafe impl Send for Running {}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(measurement) = &self.direct_measurement {
            if self
                .direct
                .as_ref()
                .is_some_and(|(generation, _)| !crate::direct_video::is_current(*generation))
            {
                measurement.surface_changed();
            }
            measurement.end_codec();
        }
        let _ = self.codec.stop();
    }
}

pub struct MediaCodecDecoder {
    colorimetry: Colorimetry,
    components: Vec<(VideoCodec, Option<String>)>,
    running: Option<Running>,
    /// What `configure` was asked for, to restart the codec differently.
    config: Option<(VideoFormat, u32, u32, u32)>,
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    failures: u32,
    /// Nothing left to try: every decode reports this.
    fatal: Option<String>,
    stats: Option<Arc<DecodeStats>>,
    gpu_output: Option<Arc<GpuOutput>>,
    /// Software decoder used when the hardware one can't be set up or its
    /// pictures can't be read (H.264).
    #[cfg(feature = "openh264")]
    fallback: Option<crate::software_h264::OpenH264Decoder>,
}

impl MediaCodecDecoder {
    /// Standalone probe for consumers that cannot query Java's codec list.
    pub fn probe(colorimetry: Colorimetry) -> Option<Self> {
        let components = [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Av1]
            .into_iter()
            .filter(|&codec| mime(codec).and_then(MediaCodec::from_decoder_type).is_some())
            .map(|codec| (codec, None))
            .collect();
        Self::new(colorimetry, components, None)
    }

    pub fn with_components(
        colorimetry: Colorimetry,
        components: Vec<(VideoCodec, String)>,
        gpu_output: Option<Arc<GpuOutput>>,
    ) -> Option<Self> {
        let components = components
            .into_iter()
            .filter(|(codec, _)| mime(*codec).is_some())
            .map(|(codec, name)| (codec, Some(name)))
            .collect();
        Self::new(colorimetry, components, gpu_output)
    }

    fn new(
        colorimetry: Colorimetry,
        components: Vec<(VideoCodec, Option<String>)>,
        gpu_output: Option<Arc<GpuOutput>>,
    ) -> Option<Self> {
        (!components.is_empty()).then_some(Self {
            colorimetry,
            components,
            running: None,
            config: None,
            last_output: None,
            last_input: None,
            failures: 0,
            fatal: None,
            stats: None,
            gpu_output,
            #[cfg(feature = "openh264")]
            fallback: None,
        })
    }

    pub fn formats(&self) -> Vec<VideoFormat> {
        let mut codecs: Vec<_> = self.components.iter().map(|(codec, _)| *codec).collect();
        if cfg!(feature = "openh264") && !codecs.contains(&VideoCodec::H264) {
            codecs.push(VideoCodec::H264);
        }
        codecs
            .into_iter()
            .map(|codec| VideoFormat {
                codec,
                ten_bit: false,
                yuv444: false,
            })
            .collect()
    }

    fn start(
        format: VideoFormat,
        width: u32,
        height: u32,
        fps: u32,
        output: Output,
        component: Option<&str>,
        stats: Option<&Arc<DecodeStats>>,
    ) -> Result<Running, String> {
        let mime = mime(format.codec).ok_or("not a MediaCodec format")?;
        let create_codec = || {
            match component {
                Some(name) => MediaCodec::from_codec_name(name),
                None => MediaCodec::from_decoder_type(mime),
            }
            .ok_or_else(|| format!("no decoder for {}", component.unwrap_or(mime)))
        };
        let mut codec = create_codec()?;
        let (w, h) = (width as i32, height as i32);
        let direct = match output {
            Output::Direct => {
                let (generation, window) =
                    crate::direct_video::wait_for_surface(DIRECT_SURFACE_WAIT).ok_or("no direct video surface")?;
                Some((generation, window))
            }
            _ => None,
        };
        let reader = match output {
            // A few buffers: one being drawn, one waiting, the codec's own.
            Output::Gpu => Some(
                ImageReader::new_with_usage(w, h, ImageFormat::PRIVATE, HardwareBufferUsage::GPU_SAMPLED_IMAGE, 4)
                    .map_err(|e| format!("GPU image reader: {e:?}"))?,
            ),
            Output::Images => {
                Some(ImageReader::new(w, h, ImageFormat::YUV_420_888, 4).map_err(|e| format!("image reader: {e:?}"))?)
            }
            Output::Buffers | Output::Direct => None,
        };
        let images = match reader {
            Some(reader) => {
                let window = reader.window().map_err(|e| format!("image reader surface: {e:?}"))?;
                Some((Arc::new(SharedReader(reader)), window))
            }
            None => None,
        };
        let media = |color: Option<i32>| {
            let mut media = Self::media_format(mime, w, h, fps);
            if let Some(color) = color {
                media.set_i32("color-format", color);
            }
            media
        };
        match (&images, &direct) {
            (_, Some((_, window))) => codec
                .configure(&media(None), Some(window), MediaCodecDirection::Decoder)
                .map_err(|e| format!("configure {mime} for direct video: {e:?}"))?,
            (Some((_, window)), None) => codec
                .configure(&media(None), Some(window), MediaCodecDirection::Decoder)
                .map_err(|e| format!("configure {mime}: {e:?}"))?,
            // NV12 if the codec will convert to it (Codec2 does), else its own.
            (None, None) => {
                if codec
                    .configure(
                        &media(Some(COLOR_YUV420_SEMI_PLANAR)),
                        None,
                        MediaCodecDirection::Decoder,
                    )
                    .is_err()
                {
                    // A failed configure may leave the component unusable.
                    drop(codec);
                    codec = create_codec()?;
                    codec
                        .configure(&media(None), None, MediaCodecDirection::Decoder)
                        .map_err(|e| format!("configure {mime} for buffer output: {e:?}"))?;
                }
            }
        }
        let direct_measurement = stats
            .filter(|_| direct.is_some())
            .map(|stats| crate::direct_video::measure(&codec, stats.clone(), fps));
        let pacing = crate::pacing::mode();
        let vsync = (direct.is_some() && pacing == Pacing::Balanced).then(VsyncClock::start);
        codec.start().map_err(|e| format!("start {mime}: {e:?}"))?;
        info!(
            "MediaCodec {} {w}x{h}@{fps} started ({output:?} output{})",
            component.unwrap_or(mime),
            if direct.is_some() {
                format!(", {} pacing", pacing.label())
            } else {
                String::new()
            }
        );
        Ok(Running {
            codec,
            output,
            images,
            layout: None,
            width: w,
            height: h,
            pts_us: 0,
            frame_us: (1_000_000 / u64::from(fps.max(1))).max(1),
            pending: VecDeque::with_capacity(MAX_PENDING_FRAMES),
            rendered: VecDeque::with_capacity(MAX_PENDING_FRAMES),
            direct: direct.map(|(generation, window)| (generation, Some(window))),
            direct_measurement,
            pacing,
            schedule: Schedule::default(),
            vsync,
        })
    }

    fn media_format(mime: &str, w: i32, h: i32, fps: u32) -> MediaFormat {
        let mut media = MediaFormat::new();
        media.set_str("mime", mime);
        media.set_i32("width", w);
        media.set_i32("height", h);
        media.set_i32("max-input-size", (w * h).max(1 << 20));
        media.set_i32("frame-rate", fps.max(1) as i32);
        // Real-time decoding, no reordering delay (Android 11+ and vendors).
        media.set_i32("priority", 0);
        media.set_i32("operating-rate", i32::from(i16::MAX));
        media.set_i32("low-latency", 1);
        for key in [
            "vendor.qti-ext-dec-low-latency.enable",
            "vendor.qti-ext-dec-picture-order.enable",
            "vendor.rtc-ext-dec-low-latency.enable",
            "vendor.low-latency.enable",
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
        ] {
            media.set_i32(key, 1);
        }
        media
    }

    fn start_output(&mut self, outputs: &[Output], reason: &str) -> Result<(), String> {
        let (format, width, height, fps) = self.config.ok_or("decoder not configured")?;
        self.running = None; // Some devices allow only one decoder at a time.
        self.last_output = None;
        self.last_input = None;
        self.failures = 0;
        let mut error = reason.to_owned();
        if let Some((_, component)) = self.components.iter().find(|(codec, _)| *codec == format.codec) {
            for &output in outputs {
                match Self::start(
                    format,
                    width,
                    height,
                    fps,
                    output,
                    component.as_deref(),
                    self.stats.as_ref(),
                ) {
                    Ok(running) => {
                        self.running = Some(running);
                        return Ok(());
                    }
                    Err(e) => {
                        warn!("MediaCodec {output:?} output for {:?} failed: {e}", format.codec);
                        error = e;
                    }
                }
            }
        }
        #[cfg(feature = "openh264")]
        if format.codec == VideoCodec::H264 {
            match crate::software_h264::OpenH264Decoder::new(self.colorimetry) {
                Ok(decoder) => {
                    warn!("MediaCodec unavailable ({error}); decoding H.264 in software");
                    self.fallback = Some(decoder);
                    return Ok(());
                }
                Err(e) => error = e,
            }
        }
        let hint = if format.codec == VideoCodec::H264 {
            ""
        } else {
            " Choose H.264 to try another decoder."
        };
        let fatal = format!("{:?} decoder failed: {reason} ({error}).{hint}", format.codec);
        self.fatal = Some(fatal.clone());
        Err(fatal)
    }

    fn fall_back(&mut self, why: &str) -> Result<(), String> {
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.output == Output::Gpu)
            && let Some(output) = &self.gpu_output
        {
            output.retire();
        }
        let gpu = self.gpu_output.as_ref().is_some_and(|output| output.available());
        let outputs: &[_] = match self.running.as_ref().map(|r| r.output) {
            Some(Output::Direct) if gpu => &[Output::Gpu, Output::Images, Output::Buffers],
            Some(Output::Direct) => &[Output::Images, Output::Buffers],
            Some(Output::Gpu) => &[Output::Images, Output::Buffers],
            Some(Output::Images) => &[Output::Buffers],
            Some(Output::Buffers) | None => &[],
        };
        warn!("MediaCodec output failed: {why}");
        self.start_output(outputs, why)
    }
}

impl Running {
    fn flush(&mut self) -> Result<(), String> {
        self.codec
            .flush()
            .map_err(|e| format!("could not flush decoder: {e:?}"))?;
        if let Some(measurement) = &self.direct_measurement {
            measurement.codec_flushed();
        }
        // Keep timestamps monotonic across resets so an image already handed
        // to the reader cannot be mistaken for an output from the new IDR.
        self.pending.clear();
        self.rendered.clear();
        self.schedule.reset();
        Ok(())
    }

    fn sync_direct_surface(&mut self) -> Result<DirectSurface, String> {
        let Some((have, current)) = &self.direct else {
            return Ok(DirectSurface::Ready);
        };
        let (generation, window) = crate::direct_video::current();
        if *have == generation {
            return Ok(if current.is_some() {
                DirectSurface::Ready
            } else {
                DirectSurface::Hidden
            });
        }
        if let Some(measurement) = &self.direct_measurement {
            measurement.surface_changed();
        }
        let Some(window) = window else {
            // A destroyed SurfaceView can leave the codec's output blocked.
            // Stop feeding it until there is a window to bind.
            self.direct = Some((generation, None));
            self.pending.clear();
            return Ok(DirectSurface::Hidden);
        };
        self.codec
            .set_output_surface(&window)
            .map_err(|e| format!("direct video surface: {e:?}"))?;
        self.flush()?;
        self.direct = Some((generation, Some(window)));
        info!("direct video: following the new surface");
        Ok(DirectSurface::Replaced)
    }

    /// Queues one access unit; false when the codec had no free input buffer.
    fn feed(&mut self, data: &[u8], mut timing: FrameTiming, wait: Duration) -> Result<bool, String> {
        let mut input = match self.codec.dequeue_input_buffer(wait) {
            Ok(DequeuedInputBufferResult::Buffer(b)) => b,
            Ok(DequeuedInputBufferResult::TryAgainLater) => return Ok(false),
            Err(e) => return Err(format!("input buffer: {e:?}")),
        };
        let buffer = input.buffer_mut();
        if data.len() > buffer.len() {
            return Err(format!("frame of {} bytes > input buffer {}", data.len(), buffer.len()));
        }
        for (dst, &src) in buffer.iter_mut().zip(data) {
            dst.write(src);
        }
        self.pts_us += self.frame_us;
        self.codec
            .queue_input_buffer(input, 0, data.len(), self.pts_us, 0)
            .map_err(|e| format!("queue input: {e:?}"))?;
        timing.codec_input_at = Instant::now();
        if self.pending.len() == MAX_PENDING_FRAMES {
            self.pending.pop_front();
        }
        self.pending.push_back((self.pts_us, timing));
        Ok(true)
    }

    fn take_timing(pending: &mut VecDeque<(u64, FrameTiming)>, pts: u64) -> Option<FrameTiming> {
        let index = pending.iter().position(|(pending_pts, _)| *pending_pts == pts)?;
        pending.remove(index).map(|(_, timing)| timing)
    }

    /// Buffer output: takes every finished picture, converts the newest
    /// into `out` (older ones go back unread) and returns its timing.
    fn drain_buffers(
        &mut self,
        colorimetry: Colorimetry,
        out: &mut DecodedFrame,
        stats: Option<&DecodeStats>,
    ) -> Result<Option<FrameTiming>, String> {
        let mut newest = None;
        loop {
            match self.codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    // Codec configuration and empty buffers do not contain pictures.
                    if buffer.info().size() <= 0 || buffer.info().flags() & BUFFER_FLAG_CODEC_CONFIG != 0 {
                        self.codec
                            .release_output_buffer(buffer, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                        continue;
                    }
                    let pts = buffer.info().presentation_time_us() as u64;
                    let Some(mut timing) = Self::take_timing(&mut self.pending, pts) else {
                        self.codec
                            .release_output_buffer(buffer, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                        continue;
                    };
                    timing.codec_output_at = Instant::now();
                    if let Some((older, _)) = newest.replace((buffer, timing)) {
                        record_superseded(stats, 1);
                        self.codec
                            .release_output_buffer(older, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let format = self.codec.output_format();
                    match BufferLayout::from_format(|key| format.i32(key), (self.width, self.height)) {
                        Ok(layout) => {
                            info!("MediaCodec buffer output: {layout:?}");
                            self.layout = Some(layout);
                        }
                        Err(e) => {
                            warn!("MediaCodec buffer output: {e}");
                            self.layout = None;
                        }
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    if let Some((buffer, _)) = newest.take() {
                        let _ = self.codec.release_output_buffer(buffer, false);
                    }
                    return Err(format!("output buffer: {e:?}"));
                }
            }
        }
        let Some((buffer, timing)) = newest else {
            return Ok(None);
        };
        let converted = (|| {
            if self.layout.is_none() {
                let format = self.codec.output_format();
                self.layout = Some(BufferLayout::from_format(
                    |key| format.i32(key),
                    (self.width, self.height),
                )?);
            }
            let layout = self.layout.as_ref().ok_or("no output buffer layout")?;
            // The NDK reports the amount of valid picture data in BufferInfo.
            let size = buffer.info().size() as usize;
            let data = buffer.buffer().get(..size).ok_or("incomplete decoder output buffer")?;
            layout.convert(data, colorimetry, out)
        })();
        self.codec
            .release_output_buffer(buffer, false)
            .map_err(|e| format!("release output: {e:?}"))?;
        converted?;
        Ok(Some(timing))
    }

    /// Release just the newest available picture to the image reader.
    fn drain_surface(&mut self, stats: Option<&DecodeStats>) -> Result<(), String> {
        let mut newest = None;
        loop {
            match self.codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let pts = buffer.info().presentation_time_us() as u64;
                    let timing = (buffer.info().flags() & BUFFER_FLAG_CODEC_CONFIG == 0)
                        .then(|| Self::take_timing(&mut self.pending, pts))
                        .flatten();
                    let Some(mut timing) = timing else {
                        self.codec
                            .release_output_buffer(buffer, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                        continue;
                    };
                    timing.codec_output_at = Instant::now();
                    if let Some((older, _, _)) = newest.replace((buffer, pts, timing)) {
                        record_superseded(stats, 1);
                        self.codec
                            .release_output_buffer(older, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let format = self.codec.output_format();
                    info!(
                        "MediaCodec output: {}x{} colour format {:?}",
                        format.i32("width").unwrap_or(0),
                        format.i32("height").unwrap_or(0),
                        format.i32("color-format")
                    );
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    if let Some((buffer, _, _)) = newest {
                        let _ = self.codec.release_output_buffer(buffer, false);
                    }
                    return Err(format!("output buffer: {e:?}"));
                }
            }
        }
        if let Some((buffer, pts, timing)) = newest {
            self.codec
                .release_output_buffer_at_time(buffer, (pts * 1_000) as i64)
                .map_err(|e| format!("release output: {e:?}"))?;
            if self.rendered.len() == MAX_PENDING_FRAMES {
                self.rendered.pop_front();
            }
            self.rendered.push_back((pts, timing));
        }
        Ok(())
    }

    /// Direct output: the newest decoded picture goes to the surface now and
    /// the system shows it at the next refresh; older ones finished in the
    /// same pass are skipped (never shown out of order).
    fn drain_direct(
        &mut self,
        out: &mut DecodedFrame,
        stats: Option<&DecodeStats>,
    ) -> Result<Option<FrameTiming>, String> {
        let visible = self
            .direct
            .as_ref()
            .is_some_and(|(generation, window)| window.is_some() && crate::direct_video::is_current(*generation));
        // Balanced keeps up to two frames from one drain, as Hestia Android's
        // queue; the other modes show only the newest.
        let keep = if self.pacing == Pacing::Balanced { 2 } else { 1 };
        let mut ready: VecDeque<(OutputBuffer<'_>, FrameTiming)> = VecDeque::with_capacity(keep + 1);
        loop {
            match self.codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let pts = buffer.info().presentation_time_us() as u64;
                    let timing = (buffer.info().flags() & BUFFER_FLAG_CODEC_CONFIG == 0)
                        .then(|| Self::take_timing(&mut self.pending, pts))
                        .flatten();
                    let Some(mut timing) = timing else {
                        self.codec
                            .release_output_buffer(buffer, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                        continue;
                    };
                    timing.codec_output_at = Instant::now();
                    if visible && let Some(measurement) = &self.direct_measurement {
                        measurement.output(timing.codec_output_at);
                    }
                    ready.push_back((buffer, timing));
                    if ready.len() > keep {
                        let (older, _) = ready.pop_front().expect("more than kept");
                        record_superseded(stats, 1);
                        let release = self.codec.release_output_buffer(older, false);
                        if let Some(measurement) = &self.direct_measurement {
                            if release.is_ok() {
                                measurement.discard_older(visible);
                            } else {
                                measurement.release_result(0, false, false);
                            }
                        }
                        release.map_err(|e| format!("release output: {e:?}"))?;
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let format = self.codec.output_format();
                    info!(
                        "MediaCodec output: {}x{} colour format {:?} (direct)",
                        format.i32("width").unwrap_or(0),
                        format.i32("height").unwrap_or(0),
                        format.i32("color-format")
                    );
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    for (buffer, _) in ready {
                        let _ = self.codec.release_output_buffer(buffer, false);
                    }
                    return Err(format!("output buffer: {e:?}"));
                }
            }
        }
        let mut newest = None;
        while let Some((buffer, timing)) = ready.pop_front() {
            let pts = buffer.info().presentation_time_us();
            if visible && let Some(measurement) = &self.direct_measurement {
                measurement.expect_callback(pts);
            }
            let release = if !visible {
                self.codec.release_output_buffer(buffer, false)
            } else {
                match self.pacing {
                    Pacing::MinLatency => self.codec.release_output_buffer(buffer, true),
                    // A zero timestamp: the display shows every frame given.
                    Pacing::MaxSmoothness => self.codec.release_output_buffer_at_time(buffer, 0),
                    Pacing::Balanced => match self.vsync.as_ref().and_then(VsyncClock::latest) {
                        Some((vsync, period)) => {
                            let target = self
                                .schedule
                                .next(monotonic_ns(), vsync, period, self.frame_us as i64 * 1000);
                            self.codec
                                .release_output_buffer_at_time(buffer, Schedule::release_time(target, period))
                        }
                        // No vsync times yet: show it at once.
                        None => self.codec.release_output_buffer(buffer, true),
                    },
                }
            };
            if let Some(measurement) = &self.direct_measurement {
                measurement.release_result(pts, visible, release.is_ok());
            }
            release.map_err(|e| format!("release output: {e:?}"))?;
            newest = Some(timing);
        }
        let Some(timing) = newest else {
            return Ok(None);
        };
        out.width = self.width as u32;
        out.height = self.height as u32;
        out.buffer = FrameBuffer::Direct;
        Ok(Some(timing))
    }

    fn poll_picture(
        &mut self,
        out: &mut DecodedFrame,
        colorimetry: Colorimetry,
        stats: Option<&DecodeStats>,
    ) -> Result<Option<FrameTiming>, String> {
        if self.output == Output::Direct {
            return self.drain_direct(out, stats);
        }
        if self.images.is_none() {
            return self.drain_buffers(colorimetry, out, stats);
        }
        self.drain_surface(stats)?;
        let (reader, _) = self.images.as_ref().expect("surface output has a reader");
        let image = match reader
            .0
            .acquire_latest_image()
            .map_err(|e| format!("acquire image: {e:?}"))?
        {
            AcquireResult::Image(image) => image,
            AcquireResult::MaxImagesAcquired => {
                if let Some(stats) = stats {
                    stats.codec_image_reader_full.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(None);
            }
            AcquireResult::NoBufferAvailable => return Ok(None),
        };
        let pts = image.timestamp().map_err(|e| format!("image timestamp: {e:?}"))? / 1_000;
        let Some(index) = self.rendered.iter().position(|(at, _)| *at == pts as u64) else {
            return Ok(None);
        };
        let (_, timing) = self
            .rendered
            .drain(..=index)
            .next_back()
            .expect("matched rendered image");
        record_superseded(stats, index as u64);
        if self.output == Output::Gpu {
            let picture = GpuPicture::new(image, reader.clone())?;
            out.width = picture.width;
            out.height = picture.height;
            out.buffer = FrameBuffer::Native(NativePicture::Android(picture));
        } else {
            convert(&image, colorimetry, (self.width, self.height), out)?;
        }
        Ok(Some(timing))
    }
}

/// CLOCK_MONOTONIC in nanoseconds, the Choreographer's and the display's
/// clock.
fn monotonic_ns() -> i64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a valid timespec to write to.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as i64 * 1_000_000_000 + now.tv_nsec as i64
}

fn record_superseded(stats: Option<&DecodeStats>, count: u64) {
    if count != 0
        && let Some(stats) = stats
    {
        stats.codec_output_superseded.fetch_add(count, Ordering::Relaxed);
    }
}

fn convert(
    image: &Image,
    colorimetry: Colorimetry,
    fallback: (i32, i32),
    out: &mut DecodedFrame,
) -> Result<(), String> {
    let err = |what: &str, e: ndk::media_error::MediaError| format!("{what}: {e:?}");
    let (width, height) = match image.crop_rect() {
        Ok(r) if r.right > r.left && r.bottom > r.top => (r.right - r.left, r.bottom - r.top),
        _ => fallback,
    };
    let plane = |i| image.plane_data(i).map_err(|e| err("plane", e));
    let row = |i| {
        image
            .plane_row_stride(i)
            .map(|s| s as usize)
            .map_err(|e| err("row stride", e))
    };
    let src = Yuv420 {
        y: plane(0)?,
        u: plane(1)?,
        v: plane(2)?,
        y_stride: row(0)?,
        u_stride: row(1)?,
        v_stride: row(2)?,
        uv_pixel_stride: image
            .plane_pixel_stride(1)
            .map(|s| s as usize)
            .map_err(|e| err("pixel stride", e))?,
        width: width as usize,
        height: height as usize,
    };
    if src.y.len() < src.y_stride * (src.height - 1) + src.width {
        return Err("luma plane smaller than the picture".into());
    }
    let pixels = out.reshape(width as u32, height as u32);
    yuv420_to_rgba(&src, colorimetry, pixels);
    Ok(())
}

impl FrameDecoder for MediaCodecDecoder {
    fn name(&self) -> &'static str {
        #[cfg(feature = "openh264")]
        if self.fallback.is_some() {
            return "OpenH264 (software; MediaCodec failed)";
        }
        match self.running.as_ref().map(|r| r.output) {
            Some(Output::Direct) => "MediaCodec (hardware, direct to the display)",
            Some(Output::Gpu) => "MediaCodec (hardware, zero-copy to the GPU)",
            Some(Output::Buffers) => "MediaCodec (hardware, buffer output)",
            _ => "MediaCodec (hardware)",
        }
    }

    fn supports(&self, format: VideoFormat) -> bool {
        !format.ten_bit
            && !format.yuv444
            && (self.components.iter().any(|(codec, _)| *codec == format.codec)
                || (cfg!(feature = "openh264") && format.codec == VideoCodec::H264))
    }

    fn configure(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        self.config = Some((format, width, height, fps));
        self.fatal = None;
        let gpu = self.gpu_output.as_ref().is_some_and(|output| output.available());
        let outputs: &[_] = match (crate::direct_video::wanted(), gpu) {
            (true, true) => &[Output::Direct, Output::Gpu, Output::Images, Output::Buffers],
            (true, false) => &[Output::Direct, Output::Images, Output::Buffers],
            (false, true) => &[Output::Gpu, Output::Images, Output::Buffers],
            (false, false) => &[Output::Images, Output::Buffers],
        };
        self.start_output(outputs, "could not initialize video output")
    }

    fn terminal_error(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String> {
        let now = Instant::now();
        self.decode_timed(frame, out, FrameTiming::new(frame, now, now))
            .map(|t| t.is_some())
    }

    fn attach_stats(&mut self, stats: Arc<DecodeStats>) {
        self.stats = Some(stats);
    }

    fn output_poll_interval(&self) -> Option<Duration> {
        let running = self.running.as_ref()?;
        if running.pending.is_empty() && running.rendered.is_empty() {
            return None;
        }
        let elapsed = self.last_input?.elapsed();
        if elapsed >= OUTPUT_STALL {
            return None;
        }
        Some(if elapsed < FAST_POLL_WINDOW {
            OUTPUT_POLL
        } else {
            SLOW_OUTPUT_POLL
        })
    }

    fn drain_output(&mut self, out: &mut DecodedFrame) -> Result<Option<FrameTiming>, String> {
        self.check_renderer()?;
        let Some(running) = self.running.as_mut() else {
            return Ok(None);
        };
        let result = match running.sync_direct_surface() {
            Ok(DirectSurface::Hidden) => return Ok(None),
            Ok(DirectSurface::Replaced) => return Err("direct video surface replaced; waiting for a keyframe".into()),
            Ok(DirectSurface::Ready) => running.poll_picture(out, self.colorimetry, self.stats.as_deref()),
            Err(e) => Err(e),
        };
        self.observe_output(result, false)
    }

    fn reset(&mut self) -> Result<(), String> {
        #[cfg(feature = "openh264")]
        if let Some(software) = &mut self.fallback {
            return software.reset();
        }
        let running = self.running.as_mut().ok_or("decoder not configured")?;
        match running.sync_direct_surface().and_then(|surface| match surface {
            DirectSurface::Ready => running.flush(),
            DirectSurface::Hidden | DirectSurface::Replaced => Ok(()),
        }) {
            Ok(()) => {}
            Err(e) => return self.fall_back(&e),
        }
        self.last_output = None;
        self.last_input = None;
        Ok(())
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut DecodedFrame,
        timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        #[cfg(feature = "openh264")]
        if let Some(software) = &mut self.fallback {
            return software.decode_timed(frame, out, timing);
        }
        if let Some(fatal) = &self.fatal {
            return Err(fatal.clone());
        }
        self.check_renderer()?;
        if let Some(running) = &mut self.running {
            match running.sync_direct_surface() {
                Ok(DirectSurface::Hidden) => {
                    self.last_input = None;
                    self.last_output = None;
                    self.failures = 0;
                    return Ok(None);
                }
                Ok(DirectSurface::Replaced) if !frame.keyframe => {
                    return Err("direct video surface replaced; waiting for a keyframe".into());
                }
                Ok(_) => {}
                Err(e) => return self.observe_output(Err(e), true),
            }
        }
        let now = Instant::now();
        if self
            .last_input
            .replace(now)
            .is_none_or(|previous| now.duration_since(previous) >= OUTPUT_STALL)
        {
            self.last_output = Some(now);
        }
        let result = self.running_decode(frame, out, timing);
        self.observe_output(result, true)
    }
}

impl MediaCodecDecoder {
    fn check_renderer(&mut self) -> Result<(), String> {
        if self.running.as_ref().is_some_and(|r| r.output == Output::Gpu)
            && let Some(why) = self.gpu_output.as_ref().and_then(|output| output.failure())
        {
            self.fall_back(&format!("GPU renderer: {why}"))?;
            return Err("switched video output; waiting for a keyframe".into());
        }
        Ok(())
    }

    fn observe_output(
        &mut self,
        result: Result<Option<FrameTiming>, String>,
        feeding: bool,
    ) -> Result<Option<FrameTiming>, String> {
        match &result {
            Ok(Some(_)) => {
                self.last_output = Some(Instant::now());
                self.failures = 0;
            }
            Ok(None) if feeding && self.last_output.is_some_and(|at| at.elapsed() >= OUTPUT_STALL) => {
                self.fall_back("packets arrive but the decoder produced no pictures for one second")?;
                return Err("switched video output; waiting for a keyframe".into());
            }
            Err(e) => {
                self.failures += 1;
                if self.failures >= FAILURES_BEFORE_FALLBACK {
                    self.fall_back(e)?;
                }
            }
            Ok(None) => {}
        }
        result
    }

    fn running_decode(
        &mut self,
        frame: &EncodedFrame,
        out: &mut DecodedFrame,
        timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        let running = self.running.as_mut().ok_or("decoder not configured")?;
        let stats = self.stats.as_deref();
        let mut picture = None;
        if !running.feed(&frame.data, timing, Duration::ZERO)? {
            if let Some(stats) = stats {
                stats.codec_input_busy.fetch_add(1, Ordering::Relaxed);
            }
            let deadline = Instant::now() + INPUT_WAIT;
            loop {
                // Release output while waiting for input capacity. Some codecs
                // cannot make progress until the client returns a buffer.
                picture = running.poll_picture(out, self.colorimetry, stats)?.or(picture);
                let wait = deadline.saturating_duration_since(Instant::now()).min(INPUT_RETRY);
                if running.feed(&frame.data, timing, wait)? {
                    break;
                }
                if wait.is_zero() {
                    if let Some(stats) = stats {
                        stats.codec_input_timeouts.fetch_add(1, Ordering::Relaxed);
                    }
                    return Err("decoder input is full after draining output".into());
                }
            }
        }
        Ok(running.poll_picture(out, self.colorimetry, stats)?.or(picture))
    }
}

/// How the codec lays out a picture in its output buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferLayout {
    /// Interleaved U/V (NV12) rather than separate planes (I420).
    semi_planar: bool,
    color_format: i32,
    stride: usize,
    slice_height: usize,
    /// The visible picture inside the buffer.
    left: usize,
    top: usize,
    width: usize,
    height: usize,
}

fn align(v: i32, to: i32) -> i32 {
    (v + to - 1) / to * to
}

impl BufferLayout {
    /// From the output format's keys (`get`), `size` when it lacks them.
    fn from_format(get: impl Fn(&str) -> Option<i32>, size: (i32, i32)) -> Result<Self, String> {
        let width = get("width").filter(|&w| w > 0).unwrap_or(size.0);
        let height = get("height").filter(|&h| h > 0).unwrap_or(size.1);
        let color_format = get("color-format").unwrap_or(COLOR_YUV420_SEMI_PLANAR);
        let semi_planar = match color_format {
            COLOR_YUV420_PLANAR | COLOR_YUV420_PACKED_PLANAR => false,
            // Flexible in a buffer is the codec's native layout; NV12 is
            // what hardware decoders use.
            COLOR_YUV420_SEMI_PLANAR
            | COLOR_YUV420_PACKED_SEMI_PLANAR
            | COLOR_YUV420_FLEXIBLE
            | COLOR_QCOM_YUV420_SEMI_PLANAR_32M => true,
            other => return Err(format!("output colour format {other:#x} isn't supported")),
        };
        let (aligned_w, aligned_h) = if color_format == COLOR_QCOM_YUV420_SEMI_PLANAR_32M {
            (align(width, 128), align(height, 32))
        } else {
            (width, height)
        };
        let stride = get("stride").filter(|&s| s >= width).unwrap_or(aligned_w);
        let slice_height = get("slice-height").filter(|&s| s >= height).unwrap_or(aligned_h);
        let left = get("crop-left").unwrap_or(0).clamp(0, width - 1);
        let top = get("crop-top").unwrap_or(0).clamp(0, height - 1);
        let right = get("crop-right")
            .filter(|&r| r > left)
            .unwrap_or(width - 1)
            .min(width - 1);
        let bottom = get("crop-bottom")
            .filter(|&b| b > top)
            .unwrap_or(height - 1)
            .min(height - 1);
        Ok(Self {
            semi_planar,
            color_format,
            stride: stride as usize,
            slice_height: slice_height as usize,
            left: left as usize,
            top: top as usize,
            width: (right - left + 1) as usize,
            height: (bottom - top + 1) as usize,
        })
    }

    /// The visible picture's planes in `data`.
    fn planes<'a>(&self, data: &'a [u8]) -> Result<Yuv420<'a>, String> {
        let (stride, left, top) = (self.stride, self.left, self.top);
        // Chroma right after the luma rows, at the reported slice height,
        // or (some decoders) right after the picture's own rows.
        let fits = |slice: usize| {
            let luma = stride * slice;
            let last_row = top + self.height - 1;
            let chroma_end = if self.semi_planar {
                luma + (last_row / 2) * stride + left + self.width
            } else {
                luma + (stride / 2) * (slice / 2) + (last_row / 2) * (stride / 2) + (left + self.width) / 2
            };
            last_row * stride + left + self.width <= data.len() && chroma_end <= data.len()
        };
        let slice = [self.slice_height, top + self.height]
            .into_iter()
            .find(|&s| fits(s))
            .ok_or_else(|| format!("output buffer of {} bytes is smaller than {self:?}", data.len()))?;
        let luma = stride * slice;
        let y = &data[top * stride + left..];
        Ok(if self.semi_planar {
            let uv = luma + (top / 2) * stride + (left & !1);
            Yuv420 {
                y,
                u: &data[uv..],
                v: &data[uv + 1..],
                y_stride: stride,
                u_stride: stride,
                v_stride: stride,
                uv_pixel_stride: 2,
                width: self.width,
                height: self.height,
            }
        } else {
            let chroma = stride / 2;
            let offset = (top / 2) * chroma + left / 2;
            let u = luma + offset;
            let v = luma + chroma * (slice / 2) + offset;
            Yuv420 {
                y,
                u: &data[u..],
                v: &data[v..],
                y_stride: stride,
                u_stride: chroma,
                v_stride: chroma,
                uv_pixel_stride: 1,
                width: self.width,
                height: self.height,
            }
        })
    }

    fn convert(&self, data: &[u8], colorimetry: Colorimetry, out: &mut DecodedFrame) -> Result<(), String> {
        let planes = self.planes(data)?;
        let pixels = out.reshape(self.width as u32, self.height as u32);
        yuv420_to_rgba(&planes, colorimetry, pixels);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(keys: &[(&str, i32)]) -> Result<BufferLayout, String> {
        BufferLayout::from_format(|key| keys.iter().find(|(k, _)| *k == key).map(|&(_, v)| v), (8, 4))
    }

    #[test]
    fn nv12_with_padding() {
        let l = layout(&[("color-format", 21), ("stride", 16), ("slice-height", 6)]).unwrap();
        assert_eq!((l.stride, l.slice_height, l.width, l.height), (16, 6, 8, 4));
        let mut data = vec![0u8; 16 * 6 * 3 / 2];
        data[16 * 6] = 7; // first U
        data[16 * 6 + 1] = 9; // first V
        let p = l.planes(&data).unwrap();
        assert_eq!((p.u[0], p.v[0], p.uv_pixel_stride, p.u_stride), (7, 9, 2, 16));
    }

    #[test]
    fn i420_planes() {
        let l = layout(&[("color-format", 19)]).unwrap();
        let mut data = vec![0u8; 8 * 4 * 3 / 2];
        data[32] = 1; // U plane
        data[32 + 8] = 2; // V plane: 4x2 chroma after U
        let p = l.planes(&data).unwrap();
        assert_eq!((p.u[0], p.v[0], p.uv_pixel_stride, p.u_stride), (1, 2, 1, 4));
    }

    #[test]
    fn qualcomm_alignment_and_crop() {
        let l = layout(&[
            ("color-format", COLOR_QCOM_YUV420_SEMI_PLANAR_32M),
            ("width", 1280),
            ("height", 736),
            ("crop-right", 1279),
            ("crop-bottom", 719),
        ])
        .unwrap();
        assert_eq!((l.stride, l.slice_height, l.width, l.height), (1280, 736, 1280, 720));
        assert!(l.planes(&vec![0; 1280 * 736 * 3 / 2]).is_ok());
    }

    #[test]
    fn chroma_right_after_the_picture() {
        // Reports a 6-row slice but packs chroma after the 4 rows.
        let l = layout(&[("color-format", 21), ("slice-height", 6)]).unwrap();
        let data = vec![0u8; 8 * 4 * 3 / 2];
        assert!(l.planes(&data).is_ok());
        assert!(l.planes(&data[..20]).is_err());
    }

    #[test]
    fn tiled_formats_are_refused() {
        assert!(layout(&[("color-format", 0x7FC0_0002)]).is_err());
    }
}
