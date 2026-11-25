//! Brings the pieces up in moonlight-common-c's order — RTSP handshake,
//! control stream connect + START, video, audio, input — and supervises them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::audio::{AudioConfig, AudioCounters, AudioReceiver};
use crate::control::{ControlEvent, ControlStream, TERMINATION_GRACEFUL};
use crate::input::{self, InputEvent};
use crate::rtsp::{self, HandshakeConfig, Negotiated};
use crate::sdp;
use crate::sink::{AudioSink, VideoSink};
use crate::video::{RoundTrip, VideoConfig, VideoCounters, VideoFeedback, VideoReceiver};
use crate::{SessionListener, StreamConfig, StreamError, StreamEvent, StreamStage, StreamStats};

enum SessionFeedback {
    Control(ControlEvent),
    Video(VideoFeedback),
    Stop,
}

const STATS_INTERVAL: Duration = Duration::from_secs(1);

pub struct StreamSession {
    negotiated: Negotiated,
    control: Arc<ControlStream>,
    video: Option<VideoReceiver>,
    audio: Option<AudioReceiver>,
    stop: Arc<AtomicBool>,
    feedback: Sender<SessionFeedback>,
    supervisor: Option<JoinHandle<()>>,
}

impl StreamSession {
    pub fn start(
        config: StreamConfig,
        video_sink: Box<dyn VideoSink>,
        audio_sink: Box<dyn AudioSink>,
        listener: Arc<dyn SessionListener>,
    ) -> Result<Self, StreamError> {
        let stage = |s: StreamStage| listener.on_event(StreamEvent::StageStarting(s));
        let fail = |s: StreamStage, e: String| {
            listener.on_event(StreamEvent::StageFailed(s, e.clone()));
            e
        };

        stage(StreamStage::Rtsp);
        let handshake = HandshakeConfig {
            session_url: config.session_url.clone(),
            server_codec_modes: config.server_codec_modes,
            remote_input_key: config.remote_input_key,
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            packet_size: config.packet_size,
            formats: config.video_formats.clone(),
            audio_channels: config.audio_channels,
            audio_channel_mask: config.audio_channel_mask,
            streaming_remotely: config.streaming_remotely,
        };
        let negotiated = rtsp::handshake_with_reference_recovery(&handshake, |format| {
            !config.show_video_through_loss && video_sink.supports_reference_frame_invalidation(format)
        })
        .map_err(|e| StreamError::Rtsp(fail(StreamStage::Rtsp, e.to_string())))?;

        stage(StreamStage::Control);
        let encrypted_control = negotiated.encryption_enabled & sdp::SS_ENC_CONTROL_V2 != 0;
        let mut control = ControlStream::connect(
            negotiated.endpoint(negotiated.control_port),
            negotiated.control_connect_data,
            config.remote_input_key,
            encrypted_control,
        )
        .map_err(|e| StreamError::Io(fail(StreamStage::Control, format!("control stream: {e}"))))?;
        let (feedback, events) = mpsc::channel();
        let control_feedback = feedback.clone();
        control
            .run_with_events(move |event| {
                let _ = control_feedback.send(SessionFeedback::Control(event));
            })
            .map_err(|e| StreamError::Io(fail(StreamStage::Control, e.to_string())))?;
        control
            .start()
            .map_err(|e| StreamError::Io(fail(StreamStage::Control, format!("START: {e}"))))?;
        let control = Arc::new(control);

        stage(StreamStage::Video);
        let announced_packet_size = if negotiated.encryption_enabled & sdp::SS_ENC_VIDEO != 0 {
            negotiated.packet_size - sdp::ENC_VIDEO_HEADER_LEN
        } else {
            negotiated.packet_size
        };
        let video_feedback = feedback.clone();
        let queued_fec_reports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queued_fec = queued_fec_reports.clone();
        let round_trip = Arc::new(RoundTrip::default());
        let video = VideoReceiver::start_with_events(
            VideoConfig {
                report_fec: negotiated.supports_fec_status,
                reference_frame_invalidation: negotiated.reference_frame_invalidation,
                remote: negotiated.endpoint(negotiated.video_port),
                ping_payload: negotiated.video_ping_payload,
                packet_size: announced_packet_size,
                format: negotiated.video_format,
                width: config.width,
                height: config.height,
                fps: config.fps,
                decrypt_key: (negotiated.encryption_enabled & sdp::SS_ENC_VIDEO != 0)
                    .then_some(config.remote_input_key),
                round_trip: round_trip.clone(),
                show_through_loss: config.show_video_through_loss,
            },
            video_sink,
            move |event| {
                // Only this video receiver enqueues block reports. Limit diagnostic traffic
                // while keeping IDR requests and session events independent of that budget.
                if matches!(event, VideoFeedback::FecStatus(_)) {
                    if queued_fec.load(Ordering::Relaxed) >= 16 {
                        return;
                    }
                    queued_fec.fetch_add(1, Ordering::Relaxed);
                }
                let _ = video_feedback.send(SessionFeedback::Video(event));
            },
        )
        .map_err(|e| StreamError::Io(fail(StreamStage::Video, e.to_string())))?;

        stage(StreamStage::Audio);
        let audio = match AudioReceiver::start(
            AudioConfig {
                remote: negotiated.endpoint(negotiated.audio_port),
                ping_payload: negotiated.audio_ping_payload,
                format: negotiated.audio_format,
                decrypt: negotiated
                    .audio_encrypted
                    .then_some((config.remote_input_key, config.remote_input_key_id)),
            },
            audio_sink,
        ) {
            Ok(a) => Some(a),
            Err(e) => {
                // Audio failing shouldn't kill an otherwise working stream.
                warn!("audio disabled: {e}");
                listener.on_event(StreamEvent::StageFailed(StreamStage::Audio, e.to_string()));
                None
            }
        };

        stage(StreamStage::Input);
        // Wiggle the mouse to wake the host display, like Moonlight does.
        for (dx, dy) in [(1, 1), (-1, -1)] {
            for p in input::encode(&InputEvent::MouseMove { dx, dy }) {
                let _ = control.send_input(&p.bytes, p.channel);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let supervisor = {
            let control = control.clone();
            let stop = stop.clone();
            let listener = listener.clone();
            let counters = video.counters.clone();
            std::thread::Builder::new()
                .name("hestia-session".into())
                .spawn(move || {
                    let mut last_loss_sent = None;
                    let mut last_stats = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        let wait = STATS_INTERVAL.saturating_sub(last_stats.elapsed());
                        let event = events.recv_timeout(wait);
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let (reason, graceful) = match event {
                            // The video loop keeps one IDR request in flight.
                            Ok(SessionFeedback::Video(VideoFeedback::NeedIdr { loss_detected_at })) => {
                                if let Err(e) = control.request_idr() {
                                    counters.recovery.send_errors.fetch_add(1, Ordering::Relaxed);
                                    warn!("could not request a video keyframe: {e}");
                                } else if let Some(loss) = loss_detected_at
                                    && Some(loss) != last_loss_sent
                                {
                                    last_loss_sent = Some(loss);
                                    let us = loss.elapsed().as_micros() as u64;
                                    counters.recovery.loss_to_send_us.fetch_add(us, Ordering::Relaxed);
                                    counters.recovery.max_loss_to_send_us.fetch_max(us, Ordering::Relaxed);
                                    counters.recovery.loss_sends.fetch_add(1, Ordering::Relaxed);
                                }
                                (None, false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::FecStatus(status))) => {
                                queued_fec_reports.fetch_sub(1, Ordering::Relaxed);
                                if control.report_fec(status).is_ok() {
                                    counters.fec_reports_sent.fetch_add(1, Ordering::Relaxed);
                                }
                                (None, false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::InvalidateReferences { first, last })) => {
                                match control.invalidate_reference_frames(first, last) {
                                    Ok(()) => counters.reference_requests_sent.fetch_add(1, Ordering::Relaxed),
                                    Err(e) => {
                                        warn!("could not request reference frame invalidation: {e}");
                                        counters.reference_request_errors.fetch_add(1, Ordering::Relaxed)
                                    }
                                };
                                (None, false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::FirstFrame)) => {
                                listener.on_event(StreamEvent::FirstFrame);
                                (None, false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::NoVideo)) => {
                                (Some("no video received from the host".into()), false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::VideoStalled)) => {
                                (Some("video stopped arriving from the host".into()), false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::SocketFailed(e))) => {
                                (Some(format!("video socket failed: {e}")), false)
                            }
                            Ok(SessionFeedback::Video(VideoFeedback::SinkFailed(reason))) => (Some(reason), false),
                            Ok(SessionFeedback::Control(ControlEvent::Terminated { code })) => {
                                let graceful = code == TERMINATION_GRACEFUL || code == 0;
                                info!("host ended the stream (code {code:#x})");
                                let reason = if graceful {
                                    "the app was closed on the host".into()
                                } else {
                                    format!("the host ended the stream (error {code:#010x})")
                                };
                                (Some(reason), graceful)
                            }
                            Ok(SessionFeedback::Control(ControlEvent::ConnectionLost)) => {
                                (Some("lost the connection to the host".into()), false)
                            }
                            Ok(SessionFeedback::Control(ControlEvent::Rumble { controller, low, high })) => {
                                listener.on_event(StreamEvent::Rumble {
                                    gamepad: controller as u8,
                                    low,
                                    high,
                                });
                                (None, false)
                            }
                            Ok(SessionFeedback::Control(ControlEvent::HdrMode { enabled })) => {
                                listener.on_event(StreamEvent::HdrMode(enabled));
                                (None, false)
                            }
                            Ok(SessionFeedback::Control(ControlEvent::TriggerRumble { .. }))
                            | Err(RecvTimeoutError::Timeout) => (None, false),
                            Ok(SessionFeedback::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                        };
                        if let Some(reason) = reason {
                            listener.on_event(StreamEvent::Terminated { reason, graceful });
                            return;
                        }
                        if last_stats.elapsed() >= STATS_INTERVAL {
                            last_stats = Instant::now();
                            // The video loop times keyframe requests from it.
                            if let Some((rtt, variance)) = control.measured_round_trip() {
                                round_trip.set(rtt, variance);
                            }
                            let (rtt, rtt_var) = control.round_trip();
                            listener.on_event(StreamEvent::Stats(StreamStats {
                                frames: counters.frames.load(Ordering::Relaxed),
                                packets: counters.packets.load(Ordering::Relaxed),
                                bytes: counters.bytes.load(Ordering::Relaxed),
                                recovered_packets: counters.recovered.load(Ordering::Relaxed),
                                lost_frames: counters.lost_frames.load(Ordering::Relaxed),
                                dropped_waiting_idr: counters.dropped_waiting_idr.load(Ordering::Relaxed),
                                dropped_waiting_rfi: counters.dropped_waiting_rfi.load(Ordering::Relaxed),
                                idr_requests: counters.recovery.requests(),
                                rtt_ms: rtt,
                                rtt_variance_ms: rtt_var,
                                host_latency_us: counters.host_latency_us.load(Ordering::Relaxed),
                                host_latency_frames: counters.host_latency_frames.load(Ordering::Relaxed),
                            }));
                        }
                    }
                })
                .map_err(|e| StreamError::Io(e.to_string()))?
        };

        listener.on_event(StreamEvent::Connected);
        Ok(Self {
            negotiated,
            control,
            video: Some(video),
            audio,
            stop,
            feedback,
            supervisor: Some(supervisor),
        })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    pub fn video_counters(&self) -> Option<Arc<VideoCounters>> {
        self.video.as_ref().map(|video| video.counters.clone())
    }

    pub fn audio_counters(&self) -> Option<Arc<AudioCounters>> {
        self.audio.as_ref().map(|audio| audio.counters.clone())
    }

    pub fn send_input(&self, event: &InputEvent) {
        for p in input::encode(event) {
            if let Err(e) = self.control.send_input(&p.bytes, p.channel) {
                warn!("input send failed: {e}");
                break;
            }
        }
    }

    pub fn request_keyframe(&self) {
        let _ = self.control.request_idr();
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.supervisor.take() {
            let _ = self.feedback.send(SessionFeedback::Stop);
            let _ = s.join();
        }
        if let Some(v) = self.video.take() {
            v.stop();
        }
        if let Some(a) = self.audio.take() {
            a.stop();
        }
        // The control stream disconnects when its last handle drops.
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_host::MockHost;
    use crate::sink::{AudioFormat, EncodedFrame, SubmitResult, VideoCodec, VideoFormat};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    struct Collect(Arc<Mutex<Vec<EncodedFrame>>>);
    impl VideoSink for Collect {
        fn supports_reference_frame_invalidation(&self, format: VideoFormat) -> bool {
            format.codec == VideoCodec::Hevc
        }

        fn setup(&mut self, _: VideoFormat, _: u32, _: u32, _: u32) -> Result<(), String> {
            Ok(())
        }
        fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
            self.0.lock().unwrap().push(frame);
            SubmitResult::Ok
        }
    }
    struct NoAudio;
    impl AudioSink for NoAudio {
        fn setup(&mut self, _: &AudioFormat) -> Result<(), String> {
            Ok(())
        }
        fn submit(&mut self, _: &[u8]) {}
    }
    struct Events(Mutex<Vec<StreamEvent>>);
    impl SessionListener for Events {
        fn on_event(&self, event: StreamEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn frame(i: usize, len: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }];
        v.extend((0..len).map(|b| ((b * 13 + i) as u8) | 1));
        v
    }

    fn test_config(host: &MockHost, key: [u8; 16]) -> StreamConfig {
        StreamConfig {
            session_url: crate::SessionUrl::parse(&format!("rtsp://127.0.0.1:{}", host.rtsp_port), "127.0.0.1")
                .unwrap(),
            server_codec_modes: 1,
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 10_000,
            packet_size: 1392,
            video_formats: vec![VideoFormat {
                codec: VideoCodec::H264,
                ten_bit: false,
                yuv444: false,
            }],
            audio_channels: 2,
            audio_channel_mask: 3,
            remote_input_key: key,
            remote_input_key_id: 7,
            streaming_remotely: false,
            show_video_through_loss: false,
        }
    }

    #[test]
    fn end_to_end_against_mock_host() {
        let key = [0x42u8; 16];
        let frames: Vec<Vec<u8>> = (0..5).map(|i| frame(i, 3000 + i * 2500)).collect();
        let host = MockHost::start(key, frames.clone(), 1392);
        let got = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Events(Mutex::new(Vec::new())));
        let session = StreamSession::start(
            test_config(&host, key),
            Box::new(Collect(got.clone())),
            Box::new(NoAudio),
            events.clone(),
        )
        .expect("session starts");
        session.send_input(&InputEvent::Key {
            key: 0x41,
            pressed: true,
            modifiers: Default::default(),
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, StreamEvent::Terminated { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        session.stop();

        let evs = events.0.lock().unwrap().clone();
        assert!(evs.contains(&StreamEvent::Connected), "{evs:?}");
        assert!(evs.contains(&StreamEvent::FirstFrame), "{evs:?}");
        assert!(
            evs.iter()
                .any(|e| matches!(e, StreamEvent::Terminated { graceful: true, .. })),
            "{evs:?}"
        );
        let received = got.lock().unwrap();
        assert_eq!(received.len(), frames.len(), "every frame recovered through FEC");
        for (r, f) in received.iter().zip(&frames) {
            assert_eq!(&r.data, f);
        }
        assert!(received[0].keyframe);

        let announce = host.announce.lock().unwrap().clone().expect("ANNOUNCE received");
        assert!(announce.contains("a=x-nv-video[0].clientViewportWd:1280 \r\n"));
        assert!(announce.contains("a=x-ss-general.encryptionEnabled:1 \r\n"));
        let messages = host.control_messages.lock().unwrap();
        let types: Vec<u16> = messages.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            &types[..2],
            &[crate::control::PT_REQUEST_IDR, crate::control::PT_START_B],
            "START A/B first"
        );
        assert!(types.contains(&crate::control::PT_PERIODIC_PING));
        let reports: Vec<_> = messages.iter().filter(|(ty, _)| *ty == 0x5502).collect();
        assert_eq!(reports.len(), frames.len());
        for (index, (_, payload)) in reports.iter().enumerate() {
            assert_eq!(payload.len(), 21);
            assert_eq!(u32::from_be_bytes(payload[..4].try_into().unwrap()), index as u32 + 1);
            assert_eq!(
                u16::from_be_bytes(payload[10..12].try_into().unwrap()),
                u16::from_be_bytes(payload[14..16].try_into().unwrap()) + 1
            );
            assert_eq!(&payload[16..18], &[0, 1]);
        }
        assert!(
            messages
                .iter()
                .any(|(t, p)| *t == crate::control::PT_INPUT && p.len() == 14),
            "keyboard input arrives decrypted"
        );
    }

    #[test]
    fn recovers_from_hevc_loss_with_reference_invalidation() {
        let key = [0x42; 16];
        let frames = (0..4)
            .map(|i| {
                let mut frame = frame(i, 3000);
                frame[4] = if i == 0 { 0x26 } else { 0x02 };
                frame
            })
            .collect();
        let host = MockHost::reference_recovery(key, frames, 1392);
        let got = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Events(Mutex::new(Vec::new())));
        let mut config = test_config(&host, key);
        config.video_formats[0].codec = VideoCodec::Hevc;
        let session = StreamSession::start(config, Box::new(Collect(got.clone())), Box::new(NoAudio), events)
            .expect("session starts");
        assert!(session.negotiated().reference_frame_invalidation);
        let deadline = Instant::now() + Duration::from_secs(3);
        while got.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        session.stop();
        let received = got.lock().unwrap();
        assert_eq!(
            received.iter().map(|frame| frame.frame_number).collect::<Vec<_>>(),
            [1, 4]
        );
        assert!(!received[1].keyframe);
        let messages = host.control_messages.lock().unwrap();
        let (_, payload) = messages
            .iter()
            .find(|(ty, _)| *ty == crate::control::PT_INVALIDATE_REF_FRAMES)
            .expect("host received a reference invalidation request");
        assert_eq!(
            payload.as_slice(),
            &[2, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            messages
                .iter()
                .filter(|(ty, _)| *ty == crate::control::PT_REQUEST_IDR)
                .count(),
            1,
            "START A only"
        );
    }

    #[test]
    fn video_stall_ends_session_while_audio_and_control_continue() {
        struct CountAudio(Arc<AtomicUsize>);
        impl AudioSink for CountAudio {
            fn setup(&mut self, _: &AudioFormat) -> Result<(), String> {
                Ok(())
            }

            fn submit(&mut self, payload: &[u8]) {
                if !payload.is_empty() {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        struct Forward(Sender<StreamEvent>);
        impl SessionListener for Forward {
            fn on_event(&self, event: StreamEvent) {
                let _ = self.0.send(event);
            }
        }

        let key = [0x42u8; 16];
        let host = MockHost::stall_video(key, vec![frame(0, 3000)], 1392);
        let video_frames = Arc::new(Mutex::new(Vec::new()));
        let audio_packets = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let session = StreamSession::start(
            test_config(&host, key),
            Box::new(Collect(video_frames.clone())),
            Box::new(CountAudio(audio_packets.clone())),
            Arc::new(Forward(tx)),
        )
        .expect("session starts");

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut audio_at_first_frame = None;
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("session should report a stall");
            match event {
                StreamEvent::FirstFrame => audio_at_first_frame = Some(audio_packets.load(Ordering::Relaxed)),
                StreamEvent::Terminated { reason, graceful } => {
                    assert!(!graceful);
                    assert_eq!(reason, "video stopped arriving from the host");
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(video_frames.lock().unwrap().len(), 1);
        assert!(audio_packets.load(Ordering::Relaxed) > audio_at_first_frame.expect("video played"));
        let types: Vec<_> = host
            .control_messages
            .lock()
            .unwrap()
            .iter()
            .map(|(ty, _)| *ty)
            .collect();
        let mut requests = types
            .iter()
            .enumerate()
            .filter(|(_, ty)| **ty == crate::control::PT_REQUEST_IDR);
        requests.next().expect("START A");
        let (probe, _) = requests.next().expect("IDR probe for stalled video");
        assert!(types[probe + 1..].contains(&crate::control::PT_PERIODIC_PING));
        session.stop();
    }
}
