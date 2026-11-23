//! `hestia stream`: launch an app and run the full streaming pipeline
//! headless, printing per-second statistics. Video is decoded (default),
//! dumped to a file or just counted; audio is counted or played (`--audio`).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use hestia_core::diagnosis;
use hestia_core::report::{StatsSample, StreamReport};
use hestia_core::stream::{SessionListener, StreamEvent, VideoCodec, VideoFormat, VideoSink};
use hestia_core::{DeviceInfo, HestiaClient, logcapture};
use hestia_media::null::{NullAudioSink, NullVideoSink};
use hestia_media::{Colorimetry, DecodeStats, DecodedFrame, DecodingSink, dump::DumpVideoSink};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    Decode,
    Dump,
    Count,
}

pub struct Options {
    pub app: String,
    pub settings: hestia_core::StreamSettings,
    pub seconds: u64,
    pub mode: VideoMode,
    pub dump: Option<PathBuf>,
    pub snapshot: Option<PathBuf>,
    pub quit_after: bool,
    /// Play audio (when built with the `audio` feature) instead of counting it.
    pub play_audio: bool,
}

enum Msg {
    Event(StreamEvent),
    Stop,
}

struct Forward(Sender<Msg>);

impl SessionListener for Forward {
    fn on_event(&self, event: StreamEvent) {
        let _ = self.0.send(Msg::Event(event));
    }
}

fn all_formats() -> Vec<VideoFormat> {
    [
        VideoCodec::Av1,
        VideoCodec::Hevc,
        VideoCodec::H264,
        VideoCodec::PyroWave,
    ]
    .into_iter()
    .map(|codec| VideoFormat {
        codec,
        ten_bit: false,
        yuv444: false,
    })
    .collect()
}

/// Saves one decoded picture as a binary PPM (any image viewer opens it).
fn write_ppm(path: &Path, frame: &DecodedFrame) -> std::io::Result<()> {
    let pixels = frame
        .rgba()
        .ok_or_else(|| std::io::Error::other("snapshot requires a CPU frame"))?;
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{} {}\n255\n", frame.width, frame.height)?;
    for px in pixels.chunks_exact(4) {
        out.write_all(&px[..3])?;
    }
    out.flush()
}

/// Saves the session report next to the app's (`<data dir>/reports`).
fn save_report(client: &HestiaClient, report: &mut StreamReport, outcome: &str) {
    report.finish(outcome);
    match report.save(&client.data_dir().join("reports")) {
        Ok(path) => eprintln!("report: {}", path.display()),
        Err(e) => eprintln!("could not save the report: {e}"),
    }
}

pub fn run(client: &HestiaClient, host_id: &str, opts: Options) -> Result<()> {
    let log_mark = logcapture::mark();
    let apps = client.apps(host_id)?;
    let app = apps
        .iter()
        .find(|a| a.id.to_string() == opts.app || a.title.eq_ignore_ascii_case(&opts.app))
        .ok_or_else(|| format!("no app matching '{}'", opts.app))?;

    // Pick the video sink before launching so a missing decoder fails early.
    let mut decode_stats: Option<Arc<DecodeStats>> = None;
    let (formats, video_sink): (Vec<VideoFormat>, Box<dyn VideoSink>) = match opts.mode {
        VideoMode::Decode => {
            let (decoder, formats) = if opts.settings.codec == hestia_core::CodecPreference::PyroWave {
                hestia_media::video_decoder::pyrowave_decoder(
                    opts.settings.width,
                    opts.settings.height,
                    opts.settings.hardware_decoding,
                    None,
                )
            } else {
                hestia_media::decoder_with(Colorimetry::default(), opts.settings.hardware_decoding)
            }?;
            let snapshot = opts.snapshot.clone();
            let started = Instant::now();
            let mut saved = false;
            let presenter = move |frame: &DecodedFrame| {
                if let Some(path) = snapshot.as_deref().filter(|_| !saved) {
                    if started.elapsed() > Duration::from_secs(2) {
                        saved = true;
                        match write_ppm(path, frame) {
                            Ok(()) => eprintln!("saved a {}x{} frame to {}", frame.width, frame.height, path.display()),
                            Err(e) => eprintln!("snapshot failed: {e}"),
                        }
                    }
                }
            };
            let sink = DecodingSink::new(decoder, Box::new(presenter));
            decode_stats = Some(sink.stats());
            (formats, Box::new(sink))
        }
        VideoMode::Dump => {
            let path = opts.dump.clone().unwrap_or_else(|| PathBuf::from("hestia-stream"));
            (all_formats(), Box::new(DumpVideoSink::new(path)))
        }
        VideoMode::Count => (all_formats(), Box::new(NullVideoSink::default())),
    };

    let launched = client.launch_with(host_id, app.id, &DeviceInfo::default(), opts.settings.clone())?;
    let host = client.stream_host(&launched);
    let mut report = StreamReport::new(format!("{} on {}", app.title, launched.host.name), log_mark);
    report.field("Client", "version", format!("hestia CLI {}", env!("CARGO_PKG_VERSION")));
    report.field(
        "Client",
        "platform",
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    );
    report.field("Client", "video", format!("{:?}", opts.mode));
    launched.add_to_report(&mut report);
    let config = match launched.stream_config(&formats) {
        Ok(config) => config,
        Err(e) => {
            save_report(client, &mut report, &format!("no usable stream configuration: {e}"));
            return Err(e.into());
        }
    };
    hestia_core::add_stream_config_to_report(&config, &format!("{:?}", opts.mode), &mut report);
    eprintln!(
        "{} '{}' on {} — {}x{}@{} {} kbps via {}",
        if launched.response.resumed {
            "resumed"
        } else {
            "launched"
        },
        app.title,
        launched.host.name,
        config.width,
        config.height,
        config.fps,
        config.bitrate_kbps,
        config.session_url.raw
    );

    let fps = config.fps;
    let (tx, rx) = mpsc::channel();
    let session = host.connect(
        config,
        video_sink,
        if opts.play_audio {
            hestia_media::default_audio_sink()
        } else {
            Box::new(NullAudioSink::default())
        },
        Arc::new(Forward(tx.clone())),
    );
    let session = match session {
        Ok(session) => session,
        Err(e) => {
            // Events emitted while starting (stage failures) are in the log.
            save_report(client, &mut report, &format!("the stream could not start: {e}"));
            return Err(e.into());
        }
    };
    session.report_negotiation(&mut report);
    let n = session.details();
    eprintln!(
        "streaming {:?} video, {} audio channels, encryption {:#x}{}",
        n.video_format.codec,
        n.audio_format.channels,
        n.encryption_enabled,
        if opts.seconds > 0 {
            format!("; stopping after {} s", opts.seconds)
        } else {
            "; press Enter to stop".into()
        }
    );

    // Enter stops the stream; EOF (no terminal) is ignored.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            if matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
                let _ = tx.send(Msg::Stop);
            }
        });
    }

    let deadline = (opts.seconds > 0).then(|| Instant::now() + Duration::from_secs(opts.seconds));
    let mut last: Option<(Instant, hestia_core::stream::StreamStats, u64)> = None;
    let mut tracker = diagnosis::DiagnosisTracker::default();
    let outcome = loop {
        let wait = deadline.map_or(Duration::from_millis(500), |d| {
            d.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500))
        });
        match rx.recv_timeout(wait) {
            Ok(Msg::Stop) => break None,
            Ok(Msg::Event(StreamEvent::Stats(stats))) => {
                let decoded = decode_stats.as_ref().map_or(0, |d| d.decoded.load(Ordering::Relaxed));
                if let Some((at, prev, prev_decoded)) = &last {
                    let secs = at.elapsed().as_secs_f64().max(0.001);
                    let received = stats.frames - prev.frames;
                    let skipped = stats.dropped_waiting_idr - prev.dropped_waiting_idr;
                    let lost = stats.lost_frames - prev.lost_frames;
                    let host_frames = stats.host_latency_frames - prev.host_latency_frames;
                    let host_ms = (host_frames > 0)
                        .then(|| (stats.host_latency_us - prev.host_latency_us) as f64 / host_frames as f64 / 1000.0);
                    let window = diagnosis::Window {
                        target_fps: fps,
                        received_fps: received as f64 / secs,
                        // Without a decoder, judge on what arrived.
                        decoded_fps: if decode_stats.is_some() {
                            (decoded - prev_decoded) as f64 / secs
                        } else {
                            received as f64 / secs
                        },
                        decode_ms: decode_stats.as_ref().map_or(0.0, |d| d.mean_decode_ms()),
                        skipped_waiting_idr_fps: skipped as f64 / secs,
                        network_drop_pct: if received + skipped + lost == 0 {
                            0.0
                        } else {
                            lost as f64 * 100.0 / (received + skipped + lost) as f64
                        },
                        display_drop_pct: 0.0,
                        host_ms,
                    };
                    let verdict = tracker.sample(&window);
                    report.sample(StatsSample {
                        fps: window.received_fps,
                        assembled_fps: (received + skipped) as f64 / secs,
                        mbps: (stats.bytes - prev.bytes) as f64 * 8.0 / secs / 1e6,
                        rtt_ms: stats.rtt_ms,
                        rtt_variance_ms: stats.rtt_variance_ms,
                        fec_recovered: stats.recovered_packets,
                        lost_frames: stats.lost_frames,
                        dropped_waiting_idr: stats.dropped_waiting_idr,
                        idr_requests: stats.idr_requests,
                        decoded_fps: decode_stats.as_ref().map(|_| window.decoded_fps),
                        direct_release_fps: None,
                        decode_ms: decode_stats.as_ref().map(|d| d.mean_decode_ms()),
                        dropped: decode_stats.as_ref().map(|d| d.dropped.load(Ordering::Relaxed)),
                        host_ms,
                        verdict: match verdict.bottleneck {
                            diagnosis::Bottleneck::None => None,
                            other => Some(other.label()),
                        },
                    });
                    let mut line = format!(
                        "{:5.1} fps  {:6.2} Mbps  rtt {:3} ms (±{})  fec-recovered {}  lost frames {}",
                        window.received_fps,
                        (stats.bytes - prev.bytes) as f64 * 8.0 / secs / 1e6,
                        stats.rtt_ms,
                        stats.rtt_variance_ms,
                        stats.recovered_packets,
                        stats.lost_frames
                    );
                    if let Some(h) = host_ms {
                        line += &format!("  host {h:.1} ms");
                    }
                    if let Some(d) = &decode_stats {
                        line += &format!(
                            "  decoded {:5.1} fps ({:.1} ms, {} dropped, {} errors)",
                            window.decoded_fps,
                            d.mean_decode_ms(),
                            d.dropped.load(Ordering::Relaxed),
                            d.errors.load(Ordering::Relaxed)
                        );
                    }
                    if verdict.bottleneck != diagnosis::Bottleneck::None {
                        line += &format!("  <- {} ({})", verdict.bottleneck.label(), verdict.key_metric);
                    }
                    eprintln!("{line}");
                }
                last = Some((Instant::now(), stats, decoded));
            }
            Ok(Msg::Event(StreamEvent::Terminated { reason, graceful })) => break Some((reason, graceful)),
            Ok(Msg::Event(StreamEvent::StageFailed(stage, e))) => {
                eprintln!("{stage:?} failed: {e}");
                report.event(format!("{stage:?} failed: {e}"));
            }
            Ok(Msg::Event(StreamEvent::FirstFrame)) => {
                eprintln!("first video frame received");
                report.first_frame();
            }
            Ok(Msg::Event(StreamEvent::HdrMode(on))) => {
                eprintln!("host HDR mode: {on}");
                report.event(format!("host HDR mode: {on}"));
            }
            Ok(Msg::Event(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break None,
        }
    };
    let video_stats = session.stop_with_stats();
    if let Some(video) = video_stats {
        report.video_transport(&video);
    }
    if let Some(d) = &decode_stats {
        report.decoder_totals(
            d.decoded.load(Ordering::Relaxed),
            d.decode_time_us.load(Ordering::Relaxed),
            d.dropped.load(Ordering::Relaxed),
        );
        report.field("Decoder totals", "decoded", d.decoded.load(Ordering::Relaxed));
        report.field("Decoder totals", "dropped", d.dropped.load(Ordering::Relaxed));
        report.field("Decoder totals", "errors", d.errors.load(Ordering::Relaxed));
        report.field(
            "Decoder totals",
            "backlog drops",
            d.dropped_backlog.load(Ordering::Relaxed),
        );
        for (stage, histogram) in d.latency_stages() {
            if let Some(summary) = histogram.summary() {
                report.field("Video latency", stage, summary);
            }
        }
        report.field("Decoder totals", "mean decode", format!("{:.2} ms", d.mean_decode_ms()));
    }
    let ending = match &outcome {
        Some((reason, _)) => reason.clone(),
        None if deadline.is_some_and(|d| Instant::now() >= d) => format!("stopped after {} s", opts.seconds),
        None => "stopped by the user".to_owned(),
    };
    save_report(client, &mut report, &ending);

    if opts.quit_after {
        client.quit(host_id)?;
        eprintln!("quit the app on the host");
    }
    match outcome {
        Some((reason, false)) => Err(reason.into()),
        Some((reason, true)) => {
            eprintln!("{reason}");
            Ok(())
        }
        None => Ok(()),
    }
}
