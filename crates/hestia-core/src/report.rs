//! Stream session reports: what was asked, what was negotiated, how the
//! stream behaved second by second, how it ended, and every log line from
//! launch to end, in one text file that is easy to share while the client is
//! a prototype.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hestia_stream::video::VideoCounters;

/// Reports kept on disk; older ones are deleted.
pub const KEEP_REPORTS: usize = 20;
/// Per-second rows written in full; longer sessions keep the first and last
/// rows.
const MAX_STAT_ROWS: usize = 600;
/// Lines of one kind (the same message, numbers aside) kept in a report's
/// log; the rest are counted at the end. A decoder failing every frame
/// would otherwise push everything else out of a shared report.
const MAX_SIMILAR_LOG_LINES: usize = 4;

/// Final video and recovery counters, captured after the session stops.
#[derive(Debug, Clone, Copy)]
pub struct VideoTransportStats {
    packets: u64,
    frames: u64,
    lost_frames: u64,
    dropped_waiting_idr: u64,
    late_packets: u64,
    late_frames: u64,
    late_buckets: [u64; hestia_stream::video::LATE_BUCKETS_MS.len() + 1],
    late_max_us: u64,
    recovered: u64,
    fec_reports_sent: u64,
    loss_sends: u64,
    loss_to_send_us: u64,
    max_loss_to_send_us: u64,
    loss_requests: u64,
    decoder_requests: u64,
    silence_requests: u64,
    retries: u64,
    send_errors: u64,
    keyframes_received: u64,
    request_to_keyframe_us: u64,
    max_request_to_keyframe_us: u64,
    keyframes_already_arriving: u64,
    request_to_keyframe_packet_us: u64,
    max_request_to_keyframe_packet_us: u64,
    keyframe_packet_to_assembly_us: u64,
    max_keyframe_packet_to_assembly_us: u64,
}

impl From<&VideoCounters> for VideoTransportStats {
    fn from(video: &VideoCounters) -> Self {
        let recovery = &video.recovery;
        Self {
            packets: video.packets.load(Ordering::Relaxed),
            frames: video.frames.load(Ordering::Relaxed),
            lost_frames: video.lost_frames.load(Ordering::Relaxed),
            dropped_waiting_idr: video.dropped_waiting_idr.load(Ordering::Relaxed),
            late_packets: video.late_packets.load(Ordering::Relaxed),
            late_frames: video.late_frames.load(Ordering::Relaxed),
            late_buckets: std::array::from_fn(|i| video.late_buckets[i].load(Ordering::Relaxed)),
            late_max_us: video.late_max_us.load(Ordering::Relaxed),
            recovered: video.recovered.load(Ordering::Relaxed),
            fec_reports_sent: video.fec_reports_sent.load(Ordering::Relaxed),
            loss_sends: recovery.loss_sends.load(Ordering::Relaxed),
            loss_to_send_us: recovery.loss_to_send_us.load(Ordering::Relaxed),
            max_loss_to_send_us: recovery.max_loss_to_send_us.load(Ordering::Relaxed),
            loss_requests: recovery.loss_requests.load(Ordering::Relaxed),
            decoder_requests: recovery.decoder_requests.load(Ordering::Relaxed),
            silence_requests: recovery.silence_requests.load(Ordering::Relaxed),
            retries: recovery.retries.load(Ordering::Relaxed),
            send_errors: recovery.send_errors.load(Ordering::Relaxed),
            keyframes_received: recovery.keyframes_received.load(Ordering::Relaxed),
            request_to_keyframe_us: recovery.request_to_keyframe_us.load(Ordering::Relaxed),
            max_request_to_keyframe_us: recovery.max_request_to_keyframe_us.load(Ordering::Relaxed),
            keyframes_already_arriving: recovery.keyframes_already_arriving.load(Ordering::Relaxed),
            request_to_keyframe_packet_us: recovery.request_to_keyframe_packet_us.load(Ordering::Relaxed),
            max_request_to_keyframe_packet_us: recovery.max_request_to_keyframe_packet_us.load(Ordering::Relaxed),
            keyframe_packet_to_assembly_us: recovery.keyframe_packet_to_assembly_us.load(Ordering::Relaxed),
            max_keyframe_packet_to_assembly_us: recovery.max_keyframe_packet_to_assembly_us.load(Ordering::Relaxed),
        }
    }
}

/// One second of stream statistics.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StatsSample {
    pub fps: f64,
    pub assembled_fps: f64,
    pub mbps: f64,
    pub rtt_ms: u32,
    pub rtt_variance_ms: u32,
    /// Running totals.
    pub fec_recovered: u64,
    pub lost_frames: u64,
    pub dropped_waiting_idr: u64,
    pub idr_requests: u64,
    /// Decoder (when the client decodes).
    pub decoded_fps: Option<f64>,
    /// Visible MediaCodec buffer releases in the Android Direct video path.
    pub direct_release_fps: Option<f64>,
    pub decode_ms: Option<f64>,
    pub dropped: Option<u64>,
    /// Host-reported processing latency for this second.
    pub host_ms: Option<f64>,
    /// Bottleneck verdict for this second ("network", "decode"…), None
    /// when healthy or not judged.
    pub verdict: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct StreamReport {
    title: String,
    started: SystemTime,
    started_at: Instant,
    log_mark: u64,
    sections: Vec<(String, Vec<(String, String)>)>,
    events: Vec<(Duration, String)>,
    samples: Vec<(Duration, StatsSample)>,
    first_frame: Option<Duration>,
    outcome: Option<(Duration, String)>,
    decoder_totals: Option<(u64, u64, u64)>,
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn split(ts: SystemTime) -> (i64, u32, u32, u64, u32) {
    let d = ts.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    (y, m, day, secs % 86_400, d.subsec_millis())
}

/// `2026-11-19 05:24:10 UTC`
pub fn utc(ts: SystemTime) -> String {
    let (y, m, d, s, _) = split(ts);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `05:24:10.123` (UTC), for log lines.
pub fn clock(ts: SystemTime) -> String {
    let (_, _, _, s, ms) = split(ts);
    format!("{:02}:{:02}:{:02}.{ms:03}", s / 3600, s / 60 % 60, s % 60)
}

/// `20261119-052410`, for file names.
fn stamp(ts: SystemTime) -> String {
    let (y, m, d, s, _) = split(ts);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    format!("+{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

fn stats_row(at: Duration, s: &StatsSample) -> String {
    let decode = match (s.decoded_fps, s.decode_ms, s.dropped) {
        (Some(f), Some(ms), Some(dr)) => format!(" | dec {f:5.1} fps {ms:5.1} ms drop {dr}"),
        _ => String::new(),
    };
    let host = s.host_ms.map(|h| format!(" | host {h:4.1} ms")).unwrap_or_default();
    let direct = s
        .direct_release_fps
        .map(|fps| format!(" | Direct release {fps:5.1} fps"))
        .unwrap_or_default();
    let verdict = s
        .verdict
        .map(|v| format!(" | <- {}", v.to_uppercase()))
        .unwrap_or_default();
    format!(
        "{} | {:5.1} fps forwarded / {:.1} assembled | {:6.2} Mbps | rtt {:3}±{:<3} | fec {:5} | lost {:4} | waiting IDR {:4} | IDR requests {:3}{decode}{direct}{host}{verdict}",
        elapsed(at),
        s.fps,
        s.assembled_fps,
        s.mbps,
        s.rtt_ms,
        s.rtt_variance_ms,
        s.fec_recovered,
        s.lost_frames,
        s.dropped_waiting_idr,
        s.idr_requests
    )
}

impl StreamReport {
    /// Starts a report now; `log_mark` is [`crate::logcapture::mark`] (or 0).
    pub fn new(title: impl Into<String>, log_mark: u64) -> Self {
        Self {
            title: title.into(),
            started: SystemTime::now(),
            started_at: Instant::now(),
            log_mark,
            sections: Vec::new(),
            events: Vec::new(),
            samples: Vec::new(),
            first_frame: None,
            outcome: None,
            decoder_totals: None,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn decoder_totals(&mut self, decoded: u64, time_us: u64, dropped: u64) {
        self.decoder_totals = Some((decoded, time_us, dropped));
    }

    pub fn video_transport(&mut self, video: &VideoTransportStats) {
        let values = [
            ("UDP packets received", video.packets),
            ("complete frames assembled", video.frames + video.dropped_waiting_idr),
            ("frames forwarded to decoder", video.frames),
            ("frames lost during assembly", video.lost_frames),
            ("lost frames whose packets still arrived", video.late_frames),
            ("packets arriving after their frame was lost", video.late_packets),
            ("complete frames skipped waiting for IDR", video.dropped_waiting_idr),
            ("packets recovered by FEC", video.recovered),
            ("FEC block reports sent to host", video.fec_reports_sent),
            ("IDR requests from assembly loss", video.loss_requests),
            ("IDR requests from decoder", video.decoder_requests),
            ("IDR probes after video silence", video.silence_requests),
            ("IDR retries", video.retries),
            ("automatic IDR control send errors", video.send_errors),
            ("automatic recoveries with a keyframe", video.keyframes_received),
        ];
        for (label, value) in values {
            self.field("Video transport and recovery", label, value);
        }
        if video.late_packets > 0 {
            let limits = hestia_stream::video::LATE_BUCKETS_MS;
            let mut parts: Vec<String> = Vec::new();
            for (i, count) in video.late_buckets.iter().enumerate() {
                let label = match i {
                    0 => format!("<{} ms", limits[0]),
                    i if i < limits.len() => format!("{}-{} ms", limits[i - 1], limits[i]),
                    _ => format!(">={} ms", limits[limits.len() - 1]),
                };
                parts.push(format!("{label}: {count}"));
            }
            self.field(
                "Video transport and recovery",
                "late packets by time after their frame was lost",
                format!("{} (max {:.2} ms)", parts.join(", "), video.late_max_us as f64 / 1000.0),
            );
        }
        if video.loss_sends > 0 {
            let mean = video.loss_to_send_us as f64 / video.loss_sends as f64 / 1000.0;
            let max = video.max_loss_to_send_us as f64 / 1000.0;
            self.field(
                "Video transport and recovery",
                "assembly loss to first IDR control send",
                format!("mean {mean:.2} ms / max {max:.2} ms ({} samples)", video.loss_sends),
            );
        }
        let recovered = video.keyframes_received;
        let first_packet_samples = recovered.saturating_sub(video.keyframes_already_arriving);
        for (label, total_us, max_us, samples) in [
            (
                "IDR request to assembled keyframe",
                video.request_to_keyframe_us,
                video.max_request_to_keyframe_us,
                recovered,
            ),
            (
                "IDR request to first packet of recovered keyframe",
                video.request_to_keyframe_packet_us,
                video.max_request_to_keyframe_packet_us,
                first_packet_samples,
            ),
            (
                "first packet to assembly for recovered keyframes",
                video.keyframe_packet_to_assembly_us,
                video.max_keyframe_packet_to_assembly_us,
                recovered,
            ),
        ] {
            if samples > 0 {
                let mean = total_us as f64 / samples as f64 / 1000.0;
                let max = max_us as f64 / 1000.0;
                self.field(
                    "Video transport and recovery",
                    label,
                    format!("mean {mean:.2} ms / max {max:.2} ms ({samples} samples)"),
                );
            }
        }
        if video.keyframes_already_arriving > 0 {
            self.field(
                "Video transport and recovery",
                "recovered keyframes already arriving before request",
                video.keyframes_already_arriving,
            );
        }
    }

    /// Adds `key: value` to a section (created on first use).
    pub fn field(&mut self, section: &str, key: &str, value: impl ToString) {
        let value = value.to_string();
        match self.sections.iter_mut().find(|(name, _)| name == section) {
            Some((_, fields)) => fields.push((key.to_owned(), value)),
            None => self.sections.push((section.to_owned(), vec![(key.to_owned(), value)])),
        }
    }

    pub fn event(&mut self, text: impl Into<String>) {
        self.events.push((self.started_at.elapsed(), text.into()));
    }

    pub fn first_frame(&mut self) {
        if self.first_frame.is_none() {
            let at = self.started_at.elapsed();
            self.first_frame = Some(at);
            self.events.push((at, "first video frame".into()));
        }
    }

    pub fn sample(&mut self, sample: StatsSample) {
        self.samples.push((self.started_at.elapsed(), sample));
    }

    /// Records how the session ended (the first call wins).
    pub fn finish(&mut self, outcome: impl Into<String>) {
        if self.outcome.is_none() {
            let at = self.started_at.elapsed();
            let outcome = outcome.into();
            self.events.push((at, format!("ended: {outcome}")));
            self.outcome = Some((at, outcome));
        }
    }

    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    fn duration(&self) -> Duration {
        self.outcome
            .as_ref()
            .map_or_else(|| self.started_at.elapsed(), |(at, _)| *at)
    }

    /// A few lines for the report screen.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut out = vec![
            format!(
                "Result: {}",
                self.outcome.as_ref().map_or("still running", |(_, o)| o.as_str())
            ),
            format!("Duration: {}", human(self.duration())),
        ];
        match self.first_frame {
            Some(at) => out.push(format!("First assembled video frame after {}", human(at))),
            None => out.push("No video frame was received".into()),
        }
        // Skip the first second (ramp-up) when there is more.
        let steady: Vec<&StatsSample> = self
            .samples
            .iter()
            .skip(usize::from(self.samples.len() > 3))
            .map(|(_, s)| s)
            .collect();
        if !steady.is_empty() {
            let n = steady.len() as f64;
            let avg = |f: fn(&StatsSample) -> f64| steady.iter().map(|s| f(s)).sum::<f64>() / n;
            let min_fps = steady.iter().map(|s| s.fps).fold(f64::MAX, f64::min);
            out.push(format!(
                "Video: {:.1} fps average (lowest {:.1}), {:.1} Mbps",
                avg(|s| s.fps),
                min_fps,
                avg(|s| s.mbps)
            ));
            out.push(format!(
                "Network: RTT {:.0} ms average, max {} ms",
                avg(|s| s.rtt_ms as f64),
                steady.iter().map(|s| s.rtt_ms).max().unwrap_or(0)
            ));
            let no_assembled = steady.iter().filter(|s| s.assembled_fps == 0.0).count();
            let no_video = steady.iter().filter(|s| s.fps == 0.0).count();
            let no_decode = steady.iter().filter(|s| s.decoded_fps == Some(0.0)).count();
            let decode_samples = steady.iter().filter(|s| s.decoded_fps.is_some()).count();
            out.push(format!(
                "No assembled video: {no_assembled}/{} sampled windows",
                steady.len()
            ));
            out.push(format!(
                "No video forwarded: {no_video}/{} sampled windows",
                steady.len()
            ));
            if decode_samples > 0 {
                out.push(format!(
                    "No decoder output: {no_decode}/{decode_samples} sampled windows"
                ));
            }
            // Seconds the per-second diagnosis flagged, by cause.
            let mut causes: Vec<(&str, usize)> = Vec::new();
            for v in steady.iter().filter_map(|s| s.verdict) {
                match causes.iter_mut().find(|(c, _)| *c == v) {
                    Some((_, n)) => *n += 1,
                    None => causes.push((v, 1)),
                }
            }
            causes.sort_by_key(|c| std::cmp::Reverse(c.1));
            out.push(if causes.is_empty() {
                "Diagnosis: no per-window bottleneck flagged".to_owned()
            } else {
                let parts: Vec<String> = causes.iter().map(|(c, n)| format!("{n} s {c}")).collect();
                format!(
                    "Diagnosis: flagged {} (of {} s sampled)",
                    parts.join(", "),
                    steady.len()
                )
            });
            if let Some(last) = steady.last() {
                out.push(format!(
                    "Losses (last sample): {} frames lost, {} complete frames skipped waiting for IDR, {} packets recovered by FEC",
                    last.lost_frames, last.dropped_waiting_idr, last.fec_recovered
                ));
            }
        }
        if let Some((decoded, time_us, dropped)) = self.decoder_totals {
            out.push(if decoded == 0 {
                format!("Decoder: no pictures produced, {dropped} frames dropped")
            } else {
                let ms = time_us as f64 / decoded as f64 / 1000.0;
                format!("Decoder: {ms:.1} ms per output on average, {decoded} pictures, {dropped} frames dropped")
            });
        }
        out
    }

    /// The full text, with `logs` (normally
    /// [`crate::logcapture::lines_since`] this report's mark).
    pub fn render_with_logs(&self, logs: &[String]) -> String {
        let mut t = String::new();
        let _ = writeln!(t, "Hestia RS stream report — {}", self.title);
        let _ = writeln!(t, "Started {}", utc(self.started));
        let _ = writeln!(t);
        let _ = writeln!(t, "== Summary");
        for line in self.summary_lines() {
            let _ = writeln!(t, "  {line}");
        }
        for (name, fields) in &self.sections {
            let _ = writeln!(t, "\n== {name}");
            let width = fields.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
            for (k, v) in fields {
                let _ = writeln!(t, "  {k:<width$}  {v}");
            }
        }
        let _ = writeln!(t, "\n== Events");
        for (at, text) in &self.events {
            let _ = writeln!(t, "  {} {text}", elapsed(*at));
        }
        let _ = writeln!(t, "\n== Statistics (per second)");
        if self.samples.is_empty() {
            let _ = writeln!(t, "  (none)");
        }
        let n = self.samples.len();
        for (i, (at, s)) in self.samples.iter().enumerate() {
            if n > MAX_STAT_ROWS && i == MAX_STAT_ROWS / 2 {
                let _ = writeln!(t, "  … {} rows omitted …", n - MAX_STAT_ROWS);
            }
            if n <= MAX_STAT_ROWS || i < MAX_STAT_ROWS / 2 || i >= n - MAX_STAT_ROWS / 2 {
                let _ = writeln!(t, "  {}", stats_row(*at, s));
            }
        }
        let (kept, left_out) = compact_log(logs);
        let hidden: usize = left_out.iter().map(|(_, n)| n).sum();
        if hidden == 0 {
            let _ = writeln!(t, "\n== Log ({} lines, times in UTC)", logs.len());
        } else {
            let _ = writeln!(
                t,
                "\n== Log ({} lines, {hidden} repeats left out, times in UTC)",
                logs.len()
            );
        }
        for line in kept {
            let _ = writeln!(t, "{line}");
        }
        if !left_out.is_empty() {
            let _ = writeln!(t, "\n== Repeats left out of the log");
            for (shape, n) in left_out.iter().take(12) {
                let _ = writeln!(t, "  {n:>6} × {shape}");
            }
        }
        t
    }

    pub fn render(&self) -> String {
        self.render_with_logs(&crate::logcapture::lines_since(self.log_mark))
    }

    /// Writes the report to `dir` and deletes the oldest beyond
    /// [`KEEP_REPORTS`].
    pub fn save(&self, dir: &Path) -> io::Result<PathBuf> {
        self.save_text(dir, &self.render())
    }

    /// Like [`save`](Self::save) with an already rendered text.
    pub fn save_text(&self, dir: &Path, text: &str) -> io::Result<PathBuf> {
        save_named(dir, "stream", self.started, text)
    }
}

/// Writes `text` to `dir/<prefix>-<stamp>.txt` and keeps the newest
/// [`KEEP_REPORTS`] files with that prefix (e.g. Hermes' own reports next
/// to stream reports).
pub fn save_named(dir: &Path, prefix: &str, at: SystemTime, text: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{prefix}-{}.txt", stamp(at)));
    std::fs::write(&path, text)?;
    prune(dir, prefix, KEEP_REPORTS);
    Ok(path)
}

fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut reports: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{prefix}-")) && n.ends_with(".txt"))
        })
        .collect();
    reports.sort();
    let excess = reports.len().saturating_sub(keep);
    for old in &reports[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

/// Shortens `text` to at most `max_bytes` by cutting whole lines out of the
/// middle: the summary at the top and the end of the log matter most.
pub fn clip_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let note_len = 96;
    let budget = max_bytes.saturating_sub(note_len);
    let head = budget / 3;
    let tail = budget - head;
    let mut head_end = head.min(text.len());
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    head_end = text[..head_end].rfind('\n').map_or(head_end, |i| i + 1);
    let mut tail_start = text.len() - tail.min(text.len());
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    tail_start = text[tail_start..].find('\n').map_or(tail_start, |i| tail_start + i + 1);
    let cut = tail_start.saturating_sub(head_end);
    format!(
        "{}[… {cut} bytes cut here; the saved report file has everything …]\n{}",
        &text[..head_end],
        &text[tail_start.max(head_end)..]
    )
}

/// The newest saved stream report in `dir`, if any.
pub fn latest(dir: &Path) -> Option<PathBuf> {
    let mut reports: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("stream-") && n.ends_with(".txt"))
        })
        .collect();
    reports.sort();
    reports.pop()
}

/// What a log line says, numbers and time aside ("decode error on frame #").
fn log_shape(line: &str) -> String {
    // Lines start with the time ("19:00:26.287 WARN  …").
    let body = line.split_once(' ').map_or(line, |(_, rest)| rest);
    let mut shape = String::with_capacity(body.len());
    let mut in_number = false;
    for c in body.chars() {
        if c.is_ascii_digit() {
            if !in_number {
                shape.push('#');
            }
            in_number = true;
        } else {
            shape.push(c);
            in_number = false;
        }
    }
    shape.chars().take(140).collect()
}

/// The log with each kind of line kept [`MAX_SIMILAR_LOG_LINES`] times;
/// a marker where one starts being left out, and how many of each kind
/// were (most first).
pub fn compact_log(logs: &[String]) -> (Vec<String>, Vec<(String, usize)>) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept = Vec::with_capacity(logs.len().min(4096));
    for line in logs {
        let count = seen.entry(log_shape(line)).or_insert(0);
        *count += 1;
        if *count <= MAX_SIMILAR_LOG_LINES {
            kept.push(line.clone());
        } else if *count == MAX_SIMILAR_LOG_LINES + 1 {
            kept.push("  … more lines like the one above are left out (counted at the end)".to_owned());
        }
    }
    let mut left_out: Vec<(String, usize)> = seen
        .into_iter()
        .filter(|&(_, n)| n > MAX_SIMILAR_LOG_LINES)
        .map(|(shape, n)| (shape, n - MAX_SIMILAR_LOG_LINES))
        .collect();
    left_out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    (kept, left_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let t = UNIX_EPOCH + Duration::from_secs(1_795_065_850) + Duration::from_millis(42);
        assert_eq!(utc(t), "2026-11-19 05:24:10 UTC");
        assert_eq!(clock(t), "05:24:10.042");
        assert_eq!(stamp(t), "20261119-052410");
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01 00:00:00 UTC");
        // Leap day.
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29 00:00:00 UTC"
        );
    }

    fn sample(fps: f64, rtt: u32, lost: u64) -> StatsSample {
        StatsSample {
            fps,
            assembled_fps: fps,
            mbps: 20.0,
            rtt_ms: rtt,
            rtt_variance_ms: 1,
            fec_recovered: 7,
            lost_frames: lost,
            dropped_waiting_idr: 0,
            idr_requests: 0,
            decoded_fps: Some(fps),
            direct_release_fps: None,
            decode_ms: Some(4.0),
            dropped: Some(0),
            host_ms: Some(3.0),
            verdict: if lost > 1 { Some("network") } else { None },
        }
    }

    #[test]
    fn sample_distinguishes_forwarded_rate_from_direct_release_rate() {
        let mut s = sample(60.0, 5, 0);
        s.direct_release_fps = Some(48.0);
        let row = stats_row(Duration::ZERO, &s);
        assert!(row.contains("60.0 fps forwarded"));
        assert!(row.contains("Direct release  48.0 fps"));
    }

    #[test]
    fn renders_everything() {
        let mut r = StreamReport::new("Desktop on cachy", 0);
        r.field("Host", "name", "cachy");
        r.field("Host", "address", "192.168.1.20");
        r.field("Negotiated", "video", "H264 1920x1080@60");
        r.event("RTSP handshake");
        r.first_frame();
        for (fps, rtt) in [(30.0, 9), (60.0, 3), (58.0, 4), (60.0, 5)] {
            r.sample(sample(fps, rtt, 1));
        }
        r.decoder_totals(10, 100_000, 2);
        r.finish("the app was closed on the host");
        r.finish("ignored: first outcome wins");
        let text = r.render_with_logs(&["05:24:10.000 INFO  hestia_stream: hello".into()]);
        for needle in [
            "Hestia RS stream report — Desktop on cachy",
            "== Host",
            "  name     cachy",
            "  video  H264 1920x1080@60",
            "RTSP handshake",
            "first video frame",
            "ended: the app was closed on the host",
            "Video: 59.3 fps average (lowest 58.0), 20.0 Mbps",
            "Network: RTT 4 ms average, max 5 ms",
            "Losses (last sample): 1 frames lost, 0 complete frames skipped waiting for IDR, 7 packets recovered by FEC",
            "Diagnosis: no per-window bottleneck flagged",
            "Decoder: 10.0 ms per output on average, 10 pictures, 2 frames dropped",
            "== Log (1 lines, times in UTC)",
            "hestia_stream: hello",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        assert!(!text.contains("ignored"));
        assert!(r.summary_lines()[0].starts_with("Result: the app was closed"));
    }

    #[test]
    fn suppressed_frames_still_count_as_assembled() {
        let mut r = StreamReport::new("IDR stall", 0);
        let mut second = sample(0.0, 17, 0);
        second.assembled_fps = 60.0;
        second.dropped_waiting_idr = 60;
        second.verdict = Some("waiting for IDR");
        r.sample(second);
        let text = r.render_with_logs(&[]);
        assert!(text.contains("No assembled video: 0/1"));
        assert!(text.contains("No video forwarded: 1/1"));
        assert!(text.contains("1 s waiting for IDR"));
    }

    #[test]
    fn repeated_log_lines_are_counted_not_kept() {
        let mut logs: Vec<String> = (0..500)
            .flat_map(|i| {
                [
                    format!(
                        "19:00:{:02}.287 WARN  hestia_media::decode: decode error on frame {i}: plane",
                        i % 60
                    ),
                    format!(
                        "19:00:{:02}.290 DEBUG hestia_stream::control: requesting IDR frame",
                        i % 60
                    ),
                ]
            })
            .collect();
        logs.insert(3, "19:00:01.000 INFO  hestia_stream::rtsp: RTSP done".into());
        logs.push("19:01:00.565 INFO  hestia_app: stream ended (graceful: false): lost the connection".into());
        let mut r = StreamReport::new("Desktop on cachy", 0);
        r.finish("lost the connection to the host");
        let text = r.render_with_logs(&logs);
        assert!(
            text.contains("== Log (1002 lines, 992 repeats left out, times in UTC)"),
            "{text}"
        );
        assert!(text.contains("RTSP done") && text.contains("lost the connection\n"));
        assert_eq!(text.matches("decode error on frame").count(), 5, "4 kept + the count");
        assert!(text.contains("   496 × WARN  hestia_media::decode: decode error on frame #: plane"));
        assert!(text.len() < 3000, "{} bytes", text.len());
    }

    #[test]
    fn counts_flagged_seconds() {
        let mut r = StreamReport::new("x", 0);
        for lost in [0, 0, 2, 3, 0] {
            r.sample(sample(60.0, 3, lost));
        }
        let summary = r.summary_lines();
        assert!(
            summary.contains(&"Diagnosis: flagged 2 s network (of 4 s sampled)".to_string()),
            "{summary:?}"
        );
        assert!(r.render_with_logs(&[]).contains("| <- NETWORK"));
    }

    #[test]
    fn long_sessions_keep_first_and_last_rows() {
        let mut r = StreamReport::new("long", 0);
        for i in 0..1000 {
            r.sample(sample(60.0, i, 0));
        }
        let text = r.render_with_logs(&[]);
        assert!(text.contains("400 rows omitted"));
        assert_eq!(text.matches(" fps forwarded / ").count(), MAX_STAT_ROWS);
    }

    #[test]
    fn clips_the_middle_on_line_boundaries() {
        let text: String = (0..2000).map(|i| format!("line {i:04} ção\n")).collect();
        assert_eq!(clip_middle("short", 100), "short");
        let clipped = clip_middle(&text, 4000);
        assert!(clipped.len() <= 4000, "{}", clipped.len());
        assert!(clipped.starts_with("line 0000 ção\n"));
        assert!(clipped.ends_with("line 1999 ção\n"));
        assert!(clipped.contains("bytes cut here"));
        // Only whole lines survive around the cut.
        for line in clipped.lines() {
            assert!(line.starts_with("line ") || line.starts_with("[…"), "{line:?}");
        }
    }

    #[test]
    fn saves_and_prunes() {
        let dir = std::env::temp_dir().join(format!("hestia-report-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(KEEP_REPORTS + 3) {
            std::fs::write(dir.join(format!("stream-20250101-0000{i:02}.txt")), "old").unwrap();
        }
        let hermes = save_named(&dir, "hermes", SystemTime::now(), "host report").unwrap();
        let r = StreamReport::new("now", 0);
        let path = r.save(&dir).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Hestia RS stream report — now")
        );
        let count = std::fs::read_dir(&dir).unwrap().count();
        // 20 stream reports + the Hermes one (pruned separately).
        assert_eq!(count, KEEP_REPORTS + 1);
        assert!(hermes.exists());
        assert_eq!(latest(&dir).as_deref(), Some(path.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
