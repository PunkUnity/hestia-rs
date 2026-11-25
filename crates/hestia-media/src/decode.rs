//! Decoding off the network thread.
//!
//! [`DecodingSink`] is the [`VideoSink`] handed to the session: `submit` only
//! queues the frame, a dedicated thread runs a [`FrameDecoder`] and gives each
//! picture to a [`FramePresenter`] (the UI, a test, a benchmark). When the
//! queue overflows or the decoder fails, frames are dropped until the next
//! keyframe and an IDR is requested from the host.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hestia_stream::{EncodedFrame, KeyframeRequester, SubmitResult, VideoFormat, VideoSink};
use log::{info, warn};

/// Pixel storage owned by a decoded picture.
pub enum FrameBuffer {
    /// RGBA8, row-major, without padding: the toolkit uploads it as an image.
    Rgba(Vec<u8>),
    /// A picture the window's GPU renderer takes at draw time (a hardware
    /// buffer, or 4:2:0 planes it converts itself).
    Native(crate::native_picture::NativePicture),
    /// Already sent to the display by the decoder (Android direct video):
    /// nothing for the UI to draw.
    Direct,
}

/// The decoder hands its buffer and its timing to the presenter together.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub buffer: FrameBuffer,
    pub frame_number: u32,
    /// When the first packet of this frame arrived.
    pub received_at: Instant,
    pub decoded_at: Instant,
    pub idr_requested_at: Option<Instant>,
    pub loss_detected_at: Option<Instant>,
}

impl Default for DecodedFrame {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            width: 0,
            height: 0,
            buffer: FrameBuffer::Rgba(Vec::new()),
            frame_number: 0,
            received_at: now,
            decoded_at: now,
            idr_requested_at: None,
            loss_detected_at: None,
        }
    }
}

impl DecodedFrame {
    pub fn rgba(&self) -> Option<&[u8]> {
        match &self.buffer {
            FrameBuffer::Rgba(data) => Some(data),
            FrameBuffer::Native(_) | FrameBuffer::Direct => None,
        }
    }

    /// Keeps a CPU allocation for reuse without retaining a native picture
    /// (dropping it releases the hardware image or returns the planes).
    pub fn discard_native(&mut self) {
        if matches!(self.buffer, FrameBuffer::Native(_) | FrameBuffer::Direct) {
            self.buffer = FrameBuffer::Rgba(Vec::new());
        }
    }

    pub fn reshape(&mut self, width: u32, height: u32) -> &mut [u8] {
        self.discard_native();
        self.width = width;
        self.height = height;
        match &mut self.buffer {
            FrameBuffer::Rgba(data) => {
                data.resize(width as usize * height as usize * 4, 0);
                data
            }
            FrameBuffer::Native(_) | FrameBuffer::Direct => unreachable!("native output was discarded"),
        }
    }
}

/// Timing and identity carried from input to its corresponding decoder output.
#[derive(Debug, Clone, Copy)]
pub struct FrameTiming {
    pub frame_number: u32,
    pub received_at: Instant,
    pub queued_at: Instant,
    pub decode_started_at: Instant,
    pub codec_input_at: Instant,
    pub codec_output_at: Instant,
    pub idr_requested_at: Option<Instant>,
    pub loss_detected_at: Option<Instant>,
}

impl FrameTiming {
    pub(crate) fn new(frame: &EncodedFrame, queued_at: Instant, started_at: Instant) -> Self {
        Self {
            frame_number: frame.frame_number,
            received_at: frame.received_at,
            queued_at,
            decode_started_at: started_at,
            codec_input_at: started_at,
            codec_output_at: started_at,
            idr_requested_at: frame.idr_requested_at,
            loss_detected_at: frame.loss_detected_at,
        }
    }
}

/// Consumes compressed access units. Decoders with asynchronous output can
/// return the timing of a previous input when that picture becomes available.
pub trait FrameDecoder: Send {
    fn name(&self) -> &'static str;
    fn supports(&self, format: VideoFormat) -> bool;
    fn supports_reference_frame_invalidation(&self, _format: VideoFormat) -> bool {
        false
    }

    /// Called once the stream's format and size are known, before the
    /// first frame (hardware decoders are created here).
    fn configure(&mut self, _format: VideoFormat, _width: u32, _height: u32, _fps: u32) -> Result<(), String> {
        Ok(())
    }
    /// Decodes one access unit; `Ok(true)` when `out` now holds a picture.
    fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String>;

    /// Discards pending output and references before decoding a recovery IDR.
    fn reset(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Time to wait for input before collecting pending output on the decoder thread.
    fn output_poll_interval(&self) -> Option<Duration> {
        None
    }

    fn drain_output(&mut self, _out: &mut DecodedFrame) -> Result<Option<FrameTiming>, String> {
        Ok(None)
    }

    fn attach_stats(&mut self, _stats: Arc<DecodeStats>) {}

    /// A failure that requesting a new keyframe cannot recover from.
    fn terminal_error(&self) -> Option<&str> {
        None
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut DecodedFrame,
        mut timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        if !self.decode(frame, out)? {
            return Ok(None);
        }
        timing.codec_output_at = Instant::now();
        Ok(Some(timing))
    }
}

/// Receives pictures on the decoder thread. A presenter handing a picture to
/// another thread can take the buffer and return an older one for reuse.
pub trait FramePresenter: Send {
    fn present(&mut self, frame: &mut DecodedFrame);
}

impl<F: FnMut(&DecodedFrame) + Send> FramePresenter for F {
    fn present(&mut self, frame: &mut DecodedFrame) {
        self(frame)
    }
}

/// Keep fine resolution for normal playback and retain the multi-second tail.
const FINE_BUCKETS: usize = 256;
const COARSE_BUCKETS: usize = 240;
const TAIL_BUCKETS: usize = 224;
const COARSE_END: usize = FINE_BUCKETS + COARSE_BUCKETS;
const LATENCY_OVERFLOW: usize = COARSE_END + TAIL_BUCKETS + 1;

#[derive(Debug)]
pub struct LatencyHistogram {
    buckets: [AtomicU64; LATENCY_OVERFLOW + 1],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl LatencyHistogram {
    pub fn record(&self, elapsed: Duration) {
        let us = elapsed.as_micros();
        let bucket = if us <= 64_000 {
            us.div_ceil(250) as usize
        } else if us <= 1_024_000 {
            FINE_BUCKETS + (us - 64_000).div_ceil(4_000) as usize
        } else {
            COARSE_END + (us - 1_024_000).div_ceil(32_000).min(TAIL_BUCKETS as u128 + 1) as usize
        };
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    pub fn summary(&self) -> Option<String> {
        let counts = self.buckets.each_ref().map(|n| n.load(Ordering::Relaxed));
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let percentile = |p: u64| {
            let target = (total * p).div_ceil(100);
            let mut cumulative = 0;
            let bucket = counts
                .iter()
                .position(|n| {
                    cumulative += *n;
                    cumulative >= target
                })
                .unwrap_or(LATENCY_OVERFLOW);
            if bucket <= FINE_BUCKETS {
                format!("{:.2}", bucket as f64 * 0.25)
            } else if bucket <= COARSE_END {
                format!("{}.00", 64 + (bucket - FINE_BUCKETS) * 4)
            } else if bucket < LATENCY_OVERFLOW {
                format!("{}.00", 1024 + (bucket - COARSE_END) * 32)
            } else {
                ">8192".to_owned()
            }
        };
        Some(format!(
            "p50 {} / p95 {} / p99 {} ms ({total} samples)",
            percentile(50),
            percentile(95),
            percentile(99)
        ))
    }
}

#[derive(Debug, Default)]
pub struct DecodeStats {
    /// The decoder in use, as it calls itself now (fallbacks change it).
    pub decoder: Mutex<&'static str>,
    pub decoded: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    /// Sum of decode + conversion time, microseconds.
    pub decode_time_us: AtomicU64,
    pub dropped_backlog: AtomicU64,
    pub resets: AtomicU64,
    pub idle_output: AtomicU64,
    pub codec_input_busy: AtomicU64,
    pub codec_input_timeouts: AtomicU64,
    pub codec_image_reader_full: AtomicU64,
    pub codec_output_superseded: AtomicU64,
    #[cfg(target_os = "android")]
    pub direct_video: crate::direct_video::DirectVideoStats,
    pub receive_to_queue: LatencyHistogram,
    pub queue_wait: LatencyHistogram,
    pub codec_feed: LatencyHistogram,
    pub codec: LatencyHistogram,
    pub frame_conversion: LatencyHistogram,
    pub recovery: RecoveryLatency,
}

impl DecodeStats {
    pub fn latency_stages(&self) -> [(&'static str, &LatencyHistogram); 6] {
        [
            ("first packet to decode queue", &self.receive_to_queue),
            ("decoder queue", &self.queue_wait),
            ("decoder worker to codec input", &self.codec_feed),
            ("codec input to output", &self.codec),
            ("codec output to frame ready", &self.frame_conversion),
            ("IDR request to first decoded picture", &self.recovery.latency),
        ]
    }

    /// Mean decode time in milliseconds over everything decoded so far.
    pub fn mean_decode_ms(&self) -> f64 {
        let n = self.decoded.load(Ordering::Relaxed);
        if n == 0 {
            return 0.0;
        }
        self.decode_time_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
    }
}

/// Count the first picture delivered for each recovery, even if a newer
/// picture replaces its keyframe at presentation time.
#[derive(Debug, Default)]
pub struct RecoveryLatency {
    last: Mutex<Option<Instant>>,
    pub latency: LatencyHistogram,
}

impl RecoveryLatency {
    pub fn record(&self, request: Option<Instant>, delivered_at: Instant) {
        if let Some(request) = request {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_none_or(|previous| request > previous) {
                *last = Some(request);
                self.latency.record(delivered_at.saturating_duration_since(request));
            }
        }
    }
}

/// Compressed frames are never dropped for being late: each one is a
/// reference for the next, so dropping one costs a keyframe (large, slow on
/// WAN links, and a stall until it arrives). The decoder catches up and only
/// the newest picture is shown. Only a backlog this deep or this old (the
/// decoder can't keep up at all) is flushed for a keyframe.
const QUEUE_DEPTH: usize = 48;
const MIN_BACKLOG_AGE: Duration = Duration::from_millis(300);
/// Frames of backlog allowed at low frame rates (more than the age above).
const MIN_BACKLOG_FRAMES: u64 = 8;

struct QueuedFrame {
    frame: EncodedFrame,
    queued_at: Instant,
    generation: u64,
}

struct Queue {
    frames: VecDeque<QueuedFrame>,
    waiting_for_keyframe: bool,
    independent_frames: bool,
    generation: u64,
    max_age: Duration,
    closed: bool,
}

impl Queue {
    fn invalidate(&mut self) -> u64 {
        let dropped = self.frames.len() as u64;
        self.frames.clear();
        self.waiting_for_keyframe = true;
        self.generation += 1;
        dropped
    }
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    stats: Arc<DecodeStats>,
}

impl Shared {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dropped_backlog(&self, count: u64) {
        self.stats.dropped.fetch_add(count, Ordering::Relaxed);
        self.stats.dropped_backlog.fetch_add(count, Ordering::Relaxed);
    }
}

pub struct DecodingSink {
    decoder: Option<Box<dyn FrameDecoder>>,
    presenter: Option<Box<dyn FramePresenter>>,
    requester: Option<KeyframeRequester>,
    failure_handler: Option<Box<dyn Fn(String) + Send>>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl DecodingSink {
    pub fn new(decoder: Box<dyn FrameDecoder>, presenter: Box<dyn FramePresenter>) -> Self {
        Self {
            decoder: Some(decoder),
            presenter: Some(presenter),
            requester: None,
            failure_handler: None,
            thread: None,
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    frames: VecDeque::with_capacity(QUEUE_DEPTH),
                    waiting_for_keyframe: true,
                    independent_frames: false,
                    generation: 0,
                    max_age: Duration::ZERO,
                    closed: false,
                }),
                ready: Condvar::new(),
                stats: Arc::new(DecodeStats::default()),
            }),
        }
    }

    pub fn stats(&self) -> Arc<DecodeStats> {
        self.shared.stats.clone()
    }
}

impl VideoSink for DecodingSink {
    fn supports_reference_frame_invalidation(&self, format: VideoFormat) -> bool {
        self.decoder
            .as_ref()
            .is_some_and(|decoder| decoder.supports_reference_frame_invalidation(format))
    }

    fn setup(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        let mut decoder = self.decoder.take().ok_or("decoder already set up")?;
        if !decoder.supports(format) {
            return Err(format!("{} cannot decode {format:?}", decoder.name()));
        }
        decoder.attach_stats(self.shared.stats.clone());
        decoder.configure(format, width, height, fps)?;
        info!(
            "video decoder: {} for {format:?} {width}x{height}@{fps}",
            decoder.name()
        );
        *self.shared.stats.decoder.lock().unwrap_or_else(|p| p.into_inner()) = decoder.name();
        let presenter = self.presenter.take().ok_or("decoder already set up")?;
        // Bursts (Wi-Fi, VPNs, keyframes) are decoded through; only a
        // decoder that can't keep up at all is reset (see QUEUE_DEPTH).
        self.shared.queue().max_age =
            Duration::from_micros(MIN_BACKLOG_FRAMES * 1_000_000 / u64::from(fps.max(1))).max(MIN_BACKLOG_AGE);
        self.shared.queue().independent_frames = format.codec == hestia_stream::VideoCodec::PyroWave;
        let shared = self.shared.clone();
        let requester = self.requester.clone();
        let failure_handler = self.failure_handler.take();
        self.thread = Some(
            std::thread::Builder::new()
                .name("hestia-decode".into())
                .spawn(move || decode_loop(decoder, presenter, shared, requester, failure_handler))
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }

    fn attach_keyframe_requester(&mut self, requester: KeyframeRequester) {
        self.requester = Some(requester);
    }

    fn attach_failure_handler(&mut self, handler: Box<dyn Fn(String) + Send>) {
        self.failure_handler = Some(handler);
    }

    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
        if self.thread.is_none() {
            return SubmitResult::Ok;
        }
        let now = Instant::now();
        let mut queue = self.shared.queue();
        if queue.closed {
            return SubmitResult::Ok;
        }
        // A pending PyroWave frame can be replaced without losing references.
        if queue.independent_frames && !queue.frames.is_empty() {
            self.shared.dropped_backlog(queue.frames.len() as u64);
            queue.frames.clear();
        }
        let overloaded = queue.frames.len() == QUEUE_DEPTH
            || queue
                .frames
                .front()
                .is_some_and(|f| now.duration_since(f.queued_at) > queue.max_age);
        if overloaded {
            let dropped = queue.invalidate();
            self.shared.dropped_backlog(dropped);
        }
        if queue.waiting_for_keyframe && !frame.keyframe {
            if overloaded {
                self.shared.dropped_backlog(1);
                return SubmitResult::NeedKeyframe;
            }
            self.shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return SubmitResult::Ok;
        }
        queue.waiting_for_keyframe = false;
        let generation = queue.generation;
        self.shared
            .stats
            .receive_to_queue
            .record(now.saturating_duration_since(frame.received_at));
        queue.frames.push_back(QueuedFrame {
            frame,
            queued_at: now,
            generation,
        });
        drop(queue);
        self.shared.ready.notify_one();
        SubmitResult::Ok
    }

    fn cleanup(&mut self) {
        self.shared.queue().closed = true;
        self.shared.ready.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for DecodingSink {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn decode_loop(
    mut decoder: Box<dyn FrameDecoder>,
    mut presenter: Box<dyn FramePresenter>,
    shared: Arc<Shared>,
    requester: Option<KeyframeRequester>,
    failure_handler: Option<Box<dyn Fn(String) + Send>>,
) {
    let mut picture = DecodedFrame::default();
    let mut logged_size = (0, 0);
    let mut decoder_generation = 0;
    loop {
        let queue = shared.queue();
        let wait_for_input = |q: &mut Queue| q.frames.is_empty() && !q.closed;
        let poll = (queue.frames.is_empty() && queue.generation == decoder_generation)
            .then(|| decoder.output_poll_interval())
            .flatten();
        let mut queue = if let Some(interval) = poll {
            shared
                .ready
                .wait_timeout_while(queue, interval, wait_for_input)
                .unwrap_or_else(|e| e.into_inner())
                .0
        } else {
            shared
                .ready
                .wait_while(queue, wait_for_input)
                .unwrap_or_else(|e| e.into_inner())
        };
        let (result, generation, input_number) = if let Some(QueuedFrame {
            frame,
            queued_at,
            generation,
        }) = queue.frames.pop_front()
        {
            let started = Instant::now();
            let queue_age = started.duration_since(queued_at);
            shared.stats.queue_wait.record(queue_age);
            if !queue.independent_frames && queue_age > queue.max_age {
                let dropped = queue.invalidate();
                drop(queue);
                warn!(
                    "decoder {} ms behind: dropping {} frames for a keyframe",
                    queue_age.as_millis(),
                    dropped + 1
                );
                shared.dropped_backlog(dropped + 1);
                if let Some(r) = &requester {
                    r.request();
                }
                continue;
            }
            drop(queue);
            let timing = FrameTiming::new(&frame, queued_at, started);
            let result = if decoder_generation != generation {
                shared.stats.resets.fetch_add(1, Ordering::Relaxed);
                decoder.reset().and_then(|()| {
                    decoder_generation = generation;
                    decoder.decode_timed(&frame, &mut picture, timing)
                })
            } else {
                decoder.decode_timed(&frame, &mut picture, timing)
            };
            (result, generation, Some(frame.frame_number))
        } else if queue.closed {
            break;
        } else if queue.generation == decoder_generation {
            drop(queue);
            (decoder.drain_output(&mut picture), decoder_generation, None)
        } else {
            continue;
        };

        if input_number.is_none() && matches!(&result, Ok(None)) && decoder.terminal_error().is_none() {
            continue;
        }
        let name = decoder.name();
        let mut current = shared.stats.decoder.lock().unwrap_or_else(|p| p.into_inner());
        if *current != name {
            info!("video decoder now: {name}");
            *current = name;
        }
        drop(current);
        let mut queue = shared.queue();
        if let Some(reason) = decoder.terminal_error() {
            let dropped = queue.invalidate();
            queue.closed = true;
            drop(queue);
            shared.stats.errors.fetch_add(1, Ordering::Relaxed);
            shared.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
            if let Some(report) = &failure_handler {
                report(reason.to_owned());
            } else {
                warn!("video decoder stopped: {reason}");
            }
            return;
        }
        if queue.generation != generation {
            // A newer keyframe may already be queued. Its generation owns recovery.
            drop(queue);
            picture.discard_native();
            if input_number.is_some() {
                shared.dropped_backlog(1);
            }
            continue;
        }
        match result {
            Ok(Some(timing)) => {
                drop(queue);
                picture.frame_number = timing.frame_number;
                picture.received_at = timing.received_at;
                picture.decoded_at = Instant::now();
                picture.idr_requested_at = timing.idr_requested_at;
                picture.loss_detected_at = timing.loss_detected_at;
                let stats = &shared.stats;
                stats.recovery.record(timing.idr_requested_at, picture.decoded_at);
                if input_number.is_none() {
                    stats.idle_output.fetch_add(1, Ordering::Relaxed);
                }
                let decode_time = picture.decoded_at.duration_since(timing.decode_started_at);
                stats
                    .decode_time_us
                    .fetch_add(decode_time.as_micros() as u64, Ordering::Relaxed);
                stats
                    .codec_feed
                    .record(timing.codec_input_at.duration_since(timing.decode_started_at));
                stats
                    .codec
                    .record(timing.codec_output_at.duration_since(timing.codec_input_at));
                stats
                    .frame_conversion
                    .record(picture.decoded_at.duration_since(timing.codec_output_at));
                stats.decoded.fetch_add(1, Ordering::Relaxed);
                if (picture.width, picture.height) != logged_size {
                    logged_size = (picture.width, picture.height);
                    info!("decoding {}x{}", picture.width, picture.height);
                }
                presenter.present(&mut picture);
                picture.discard_native();
            }
            Ok(None) => {}
            Err(e) => {
                let independent = queue.independent_frames;
                let dropped = if independent { 1 } else { queue.invalidate() };
                drop(queue);
                picture.discard_native();
                // The first few in full, then a reminder now and then: a
                // decoder failing every frame would bury the log.
                let errors = shared.stats.errors.fetch_add(1, Ordering::Relaxed) + 1;
                if errors <= 5 || errors.is_multiple_of(100) {
                    if let Some(frame) = input_number {
                        warn!("decode error on frame {frame} ({errors} so far): {e}");
                    } else {
                        warn!("decode output error ({errors} so far): {e}");
                    }
                }
                shared.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
                if !independent && let Some(r) = &requester {
                    r.request();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hestia_stream::VideoCodec;
    use std::sync::mpsc;

    /// "Decodes" a frame into a 2x1 picture whose red channel is the frame
    /// number; fails on frames listed in `fail`.
    struct Fake {
        fail: Vec<u32>,
    }

    impl FrameDecoder for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn supports(&self, f: VideoFormat) -> bool {
            f.codec == VideoCodec::H264
        }
        fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String> {
            if self.fail.contains(&frame.frame_number) {
                return Err("corrupt".into());
            }
            out.reshape(2, 1)[0] = frame.frame_number as u8;
            Ok(true)
        }
    }

    fn frame(n: u32, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            frame_number: n,
            keyframe,
            data: vec![0, 0, 0, 1],
            received_at: Instant::now(),
            idr_requested_at: None,
            loss_detected_at: None,
            host_latency: None,
        }
    }

    const H264: VideoFormat = VideoFormat {
        codec: VideoCodec::H264,
        ten_bit: false,
        yuv444: false,
    };

    #[test]
    fn collects_delayed_output_without_another_input_frame() {
        #[derive(Default)]
        struct Delayed(Option<FrameTiming>);

        impl FrameDecoder for Delayed {
            fn name(&self) -> &'static str {
                "delayed"
            }
            fn supports(&self, _: VideoFormat) -> bool {
                true
            }
            fn decode(&mut self, _: &EncodedFrame, _: &mut DecodedFrame) -> Result<bool, String> {
                unreachable!("the worker uses the timed API")
            }
            fn decode_timed(
                &mut self,
                _: &EncodedFrame,
                _: &mut DecodedFrame,
                timing: FrameTiming,
            ) -> Result<Option<FrameTiming>, String> {
                self.0 = Some(timing);
                Ok(None)
            }
            fn output_poll_interval(&self) -> Option<Duration> {
                self.0.map(|_| Duration::from_millis(1))
            }
            fn drain_output(&mut self, out: &mut DecodedFrame) -> Result<Option<FrameTiming>, String> {
                out.reshape(2, 1);
                Ok(self.0.take().map(|mut timing| {
                    timing.codec_output_at = Instant::now();
                    timing
                }))
            }
        }

        let (presented, pictures) = mpsc::channel();
        let mut sink = DecodingSink::new(
            Box::new(Delayed::default()),
            Box::new(move |frame: &DecodedFrame| {
                presented
                    .send((frame.frame_number, frame.idr_requested_at, frame.loss_detected_at))
                    .unwrap()
            }),
        );
        let stats = sink.stats();
        sink.setup(H264, 2, 1, 60).unwrap();
        let mut keyframe = frame(17, true);
        let requested = Instant::now();
        let loss = requested - Duration::from_millis(80);
        keyframe.idr_requested_at = Some(requested);
        keyframe.loss_detected_at = Some(loss);
        assert_eq!(sink.submit(keyframe), SubmitResult::Ok);
        assert_eq!(
            pictures.recv_timeout(Duration::from_secs(2)).unwrap(),
            (17, Some(requested), Some(loss))
        );
        sink.cleanup();
        assert_eq!(stats.decoded.load(Ordering::Relaxed), 1);
        assert_eq!(stats.idle_output.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn decodes_and_recovers_from_errors() {
        let shown = Arc::new(Mutex::new(Vec::new()));
        let (requested, request) = mpsc::channel();
        let s = shown.clone();
        let mut sink = DecodingSink::new(
            Box::new(Fake { fail: vec![3] }),
            Box::new(move |f: &DecodedFrame| s.lock().unwrap().push(f.rgba().unwrap()[0] as u32)),
        );
        sink.attach_keyframe_requester(KeyframeRequester::new(move || {
            requested.send(()).unwrap();
        }));
        sink.setup(H264, 1280, 720, 60).unwrap();

        // P-frame before the first keyframe is dropped.
        assert_eq!(sink.submit(frame(1, false)), SubmitResult::Ok);
        sink.submit(frame(2, true));
        sink.submit(frame(3, false));
        request.recv_timeout(Duration::from_secs(1)).unwrap();
        for n in 4..=6 {
            sink.submit(frame(n, false));
        }
        sink.submit(frame(7, true));
        sink.submit(frame(8, false));
        sink.cleanup();

        // 3 fails → 4..6 are dropped until keyframe 7.
        assert_eq!(*shown.lock().unwrap(), vec![2, 7, 8]);
        assert!(request.try_recv().is_err());
        let stats = sink.stats();
        assert_eq!(stats.errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.decoded.load(Ordering::Relaxed), 3);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 4);
    }

    /// A decoder slower than real time for a while (a burst arrives at
    /// once): every frame is still decoded, nothing is dropped, no keyframe
    /// is asked for.
    #[test]
    fn bursts_are_decoded_through() {
        struct Slow;
        impl FrameDecoder for Slow {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn supports(&self, _f: VideoFormat) -> bool {
                true
            }
            fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String> {
                std::thread::sleep(Duration::from_millis(5));
                out.reshape(2, 1)[0] = frame.frame_number as u8;
                Ok(true)
            }
        }
        let shown = Arc::new(Mutex::new(Vec::new()));
        let (requested, request) = mpsc::channel();
        let s = shown.clone();
        let mut sink = DecodingSink::new(
            Box::new(Slow),
            Box::new(move |f: &DecodedFrame| s.lock().unwrap().push(f.rgba().unwrap()[0] as u32)),
        );
        sink.attach_keyframe_requester(KeyframeRequester::new(move || {
            requested.send(()).unwrap();
        }));
        sink.setup(H264, 1280, 720, 60).unwrap();
        // 20 frames at once: 100 ms of decoding, six frame times behind.
        sink.submit(frame(1, true));
        for n in 2..=20 {
            assert_eq!(sink.submit(frame(n, false)), SubmitResult::Ok);
        }
        sink.cleanup();
        assert_eq!(*shown.lock().unwrap(), (1..=20).collect::<Vec<_>>());
        assert!(request.try_recv().is_err(), "no keyframe needed");
        assert_eq!(sink.stats().dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reports_backlog_over_64_ms() {
        let histogram = LatencyHistogram::default();
        histogram.record(Duration::from_millis(300));
        assert_eq!(
            histogram.summary().as_deref(),
            Some("p50 300.00 / p95 300.00 / p99 300.00 ms (1 samples)")
        );
    }

    #[test]
    fn reports_recovery_latency_beyond_one_second_once_per_request() {
        let recovery = RecoveryLatency::default();
        let request = Instant::now();
        recovery.record(Some(request), request + Duration::from_millis(1937));
        recovery.record(Some(request), request + Duration::from_secs(3));
        assert_eq!(
            recovery.latency.summary().unwrap(),
            "p50 1952.00 / p95 1952.00 / p99 1952.00 ms (1 samples)"
        );
    }

    #[test]
    fn terminal_failure_reports_once_without_requesting_an_idr() {
        struct Broken(bool);
        impl FrameDecoder for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn supports(&self, _format: VideoFormat) -> bool {
                true
            }
            fn decode(&mut self, _frame: &EncodedFrame, _out: &mut DecodedFrame) -> Result<bool, String> {
                self.0 = true;
                Err("no usable video output".into())
            }
            fn terminal_error(&self) -> Option<&str> {
                self.0.then_some("no usable video output")
            }
        }

        let (send_error, errors) = mpsc::channel();
        let (send_idr, idrs) = mpsc::channel();
        let mut sink = DecodingSink::new(Box::new(Broken(false)), Box::new(|_: &DecodedFrame| {}));
        sink.attach_failure_handler(Box::new(move |reason| send_error.send(reason).unwrap()));
        sink.attach_keyframe_requester(KeyframeRequester::new(move || send_idr.send(()).unwrap()));
        sink.setup(H264, 1280, 720, 60).unwrap();
        sink.submit(frame(1, true));
        assert_eq!(
            errors.recv_timeout(Duration::from_secs(1)).unwrap(),
            "no usable video output"
        );
        sink.submit(frame(2, true));
        sink.cleanup();
        assert!(errors.try_recv().is_err());
        assert!(idrs.try_recv().is_err());
    }

    #[test]
    fn rejects_unsupported_codec() {
        let mut sink = DecodingSink::new(Box::new(Fake { fail: vec![] }), Box::new(|_: &DecodedFrame| {}));
        let hevc = VideoFormat {
            codec: VideoCodec::Hevc,
            ..H264
        };
        assert!(sink.setup(hevc, 1920, 1080, 60).is_err());
    }
}
