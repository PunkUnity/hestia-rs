//! Turns a second of stream statistics into a plain-language verdict: is
//! the stream healthy, and if not, is it the network, the decoder, the
//! presentation or the host? A session tracks consecutive throughput deficits
//! and checks the assembler's recovery state before scoring competing causes.

/// A window needs this many frames before it is judged.
const MIN_FRAMES_TO_JUDGE: f64 = 10.0;
/// Drop percentage above which loss is a real problem.
const DROP_PERCENT_THRESHOLD: f64 = 2.0;
const HIGH_RTT_MS: u32 = 250;
const SLOW_ARRIVAL_FRACTION: f64 = 0.8;
/// Host processing latency considered high.
const HOST_LATENCY_HIGH_MS: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bottleneck {
    #[default]
    None,
    Network,
    WaitingForIdr,
    Decode,
    Render,
    Host,
}

impl Bottleneck {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "ok",
            Self::WaitingForIdr => "waiting for IDR",
            Self::Network => "network",
            Self::Decode => "decode",
            Self::Render => "client frame drops",
            Self::Host => "host",
        }
    }
}

/// One second of measurements, as rates and per-frame averages.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Window {
    pub target_fps: u32,
    /// Complete frames forwarded to the decoder.
    pub received_fps: f64,
    pub decoded_fps: f64,
    pub skipped_waiting_idr_fps: f64,
    /// Average decode (+ conversion) time per decoded frame.
    pub decode_ms: f64,
    /// Frames lost on the network, as a share of all frames.
    pub network_drop_pct: f64,
    pub rtt_ms: u32,
    /// Decoded frames superseded by a newer one before release or draw.
    pub display_drop_pct: f64,
    /// Average host processing latency, when the host reports it.
    pub host_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Diagnosis {
    pub bottleneck: Bottleneck,
    /// One sentence for the user; empty while there is too little data.
    pub summary: &'static str,
    /// The number behind the verdict, e.g. "3.1% network drops".
    pub key_metric: String,
}

#[derive(Default)]
pub struct DiagnosisTracker {
    previous_deficit: bool,
}

impl DiagnosisTracker {
    pub fn sample(&mut self, w: &Window) -> Diagnosis {
        let deficit = w.received_fps >= MIN_FRAMES_TO_JUDGE && w.decoded_fps < w.received_fps * 0.95;
        let sustained = self.previous_deficit && deficit;
        self.previous_deficit = deficit;
        classify(w, sustained)
    }
}

pub fn diagnose(w: &Window) -> Diagnosis {
    DiagnosisTracker::default().sample(w)
}

fn classify(w: &Window, sustained_deficit: bool) -> Diagnosis {
    if w.skipped_waiting_idr_fps > 0.0 && w.received_fps < MIN_FRAMES_TO_JUDGE {
        return waiting_for_idr(w);
    }
    // Frames arrive but none come out: the decoder is failing, not slow.
    if w.decoded_fps < 0.5 && w.received_fps >= MIN_FRAMES_TO_JUDGE {
        return Diagnosis {
            bottleneck: Bottleneck::Decode,
            summary: "The video decoder isn't producing pictures. If this lasts, turn off Hardware decoding in the settings.",
            key_metric: format!("0 of {:.0} fps decoded", w.received_fps),
        };
    }
    let assembled_fps = w.received_fps + w.skipped_waiting_idr_fps;
    if w.rtt_ms >= HIGH_RTT_MS
        && w.network_drop_pct < DROP_PERCENT_THRESHOLD
        && assembled_fps < f64::from(w.target_fps) * SLOW_ARRIVAL_FRACTION
        && w.host_ms.unwrap_or(0.0) < HOST_LATENCY_HIGH_MS
    {
        return Diagnosis {
            bottleneck: Bottleneck::Network,
            summary: "Network latency is high while video arrivals have slowed.",
            key_metric: format!(
                "RTT {} ms, {assembled_fps:.0} of {} fps assembled",
                w.rtt_ms, w.target_fps
            ),
        };
    }
    if w.decoded_fps < MIN_FRAMES_TO_JUDGE && !sustained_deficit && w.network_drop_pct < DROP_PERCENT_THRESHOLD {
        return Diagnosis::default();
    }
    let budget_ms = 1000.0 / f64::from(if w.target_fps > 0 { w.target_fps } else { 60 });

    let network = w.network_drop_pct / DROP_PERCENT_THRESHOLD;
    let mut decode = 0.0_f64;
    // Decoded throughput short of what arrived is a strong decode signal.
    if sustained_deficit {
        decode = (w.decode_ms / budget_ms).max(1.5);
    }
    let render = w.display_drop_pct / DROP_PERCENT_THRESHOLD;
    let host = w.host_ms.unwrap_or(0.0) / HOST_LATENCY_HIGH_MS;

    let mut dominant = Bottleneck::None;
    let mut best = 1.0;
    if network >= best {
        best = network;
        dominant = Bottleneck::Network;
    }
    for (score, cause) in [
        (
            (w.skipped_waiting_idr_fps * 100.0 / (w.received_fps + w.skipped_waiting_idr_fps).max(1.0))
                / DROP_PERCENT_THRESHOLD,
            Bottleneck::WaitingForIdr,
        ),
        (decode, Bottleneck::Decode),
        (render, Bottleneck::Render),
        (host, Bottleneck::Host),
    ] {
        if score > best {
            best = score;
            dominant = cause;
        }
    }

    match dominant {
        Bottleneck::None => Diagnosis {
            bottleneck: dominant,
            summary: "Stream is healthy.",
            key_metric: String::new(),
        },
        Bottleneck::WaitingForIdr => waiting_for_idr(w),
        Bottleneck::Network => Diagnosis {
            bottleneck: dominant,
            summary: "The network is dropping frames. Lower the bitrate, or move closer to the access point / use a wired connection.",
            key_metric: format!("{:.1}% network drops", w.network_drop_pct),
        },
        Bottleneck::Decode => Diagnosis {
            bottleneck: dominant,
            summary: "The decoder is falling behind. Lower the resolution or frame rate.",
            key_metric: format!("decoding {:.0} of {:.0} fps", w.decoded_fps, w.received_fps),
        },
        Bottleneck::Render => Diagnosis {
            bottleneck: dominant,
            summary: "The client replaced decoded frames before presentation.",
            key_metric: format!("{:.1}% decoded frames replaced", w.display_drop_pct),
        },
        Bottleneck::Host => Diagnosis {
            bottleneck: dominant,
            summary: "The host is slow to produce frames. This is a server-side bottleneck, not this device.",
            key_metric: format!("host {:.1} ms/frame", w.host_ms.unwrap_or(0.0)),
        },
    }
}

fn waiting_for_idr(w: &Window) -> Diagnosis {
    Diagnosis {
        bottleneck: Bottleneck::WaitingForIdr,
        summary: "Video is waiting for a keyframe to resume.",
        key_metric: format!("{:.1} complete frames/s skipped", w.skipped_waiting_idr_fps),
    }
}

/// The last ~5 minutes of per-second verdicts.
#[derive(Debug, Clone)]
pub struct SpikeHistory {
    samples: std::collections::VecDeque<Bottleneck>,
    capacity: usize,
}

impl Default for SpikeHistory {
    fn default() -> Self {
        Self::new(300)
    }
}

impl SpikeHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn record(&mut self, b: Bottleneck) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(b);
    }

    pub fn spikes(&self) -> usize {
        self.samples.iter().filter(|b| **b != Bottleneck::None).count()
    }

    /// "3 spikes in the last ~5 min (mostly network)", or None when clean.
    pub fn summarize(&self) -> Option<String> {
        let spikes = self.spikes();
        if spikes == 0 {
            return None;
        }
        let causes = [
            Bottleneck::WaitingForIdr,
            Bottleneck::Network,
            Bottleneck::Decode,
            Bottleneck::Render,
            Bottleneck::Host,
        ];
        let count = |c: Bottleneck| self.samples.iter().filter(|b| **b == c).count();
        let top = causes
            .iter()
            .copied()
            .max_by_key(|c| count(*c))
            .unwrap_or(Bottleneck::Network);
        let minutes = self.samples.len().div_ceil(60);
        Some(format!(
            "{spikes} spike{} in the last ~{minutes} min (mostly {})",
            if spikes == 1 { "" } else { "s" },
            top.label()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Window {
        Window {
            target_fps: 60,
            received_fps: 60.0,
            decoded_fps: 60.0,
            decode_ms: 3.0,
            skipped_waiting_idr_fps: 0.0,
            network_drop_pct: 0.0,
            rtt_ms: 0,
            display_drop_pct: 0.0,
            host_ms: Some(4.0),
        }
    }

    #[test]
    fn healthy_and_too_little_data() {
        assert_eq!(diagnose(&healthy()).bottleneck, Bottleneck::None);
        assert_eq!(diagnose(&healthy()).summary, "Stream is healthy.");
        let starting = Window {
            decoded_fps: 4.0,
            ..healthy()
        };
        assert_eq!(diagnose(&starting), Diagnosis::default());
        let nothing_arrives = Window {
            received_fps: 0.0,
            decoded_fps: 0.0,
            ..healthy()
        };
        assert_eq!(diagnose(&nothing_arrives), Diagnosis::default());
    }

    #[test]
    fn a_decoder_making_no_pictures_is_called_out() {
        let d = diagnose(&Window {
            received_fps: 38.0,
            decoded_fps: 0.0,
            decode_ms: 0.0,
            ..healthy()
        });
        assert_eq!(d.bottleneck, Bottleneck::Decode);
        assert_eq!(d.key_metric, "0 of 38 fps decoded");
        assert!(d.summary.contains("Hardware decoding"));
    }

    #[test]
    fn classifies_each_cause() {
        let net = diagnose(&Window {
            network_drop_pct: 5.0,
            ..healthy()
        });
        assert_eq!(net.bottleneck, Bottleneck::Network);
        assert_eq!(net.key_metric, "5.0% network drops");

        let mut tracker = DiagnosisTracker::default();
        let steady = Window {
            decode_ms: 14.0,
            ..healthy()
        };
        assert_eq!(tracker.sample(&steady).bottleneck, Bottleneck::None);
        let behind = Window {
            decoded_fps: 45.0,
            ..steady
        };
        assert_eq!(tracker.sample(&behind).bottleneck, Bottleneck::None);
        assert_eq!(tracker.sample(&behind).bottleneck, Bottleneck::Decode);
        assert_eq!(tracker.sample(&steady).bottleneck, Bottleneck::None);
        let stalled = Window {
            received_fps: 0.0,
            decoded_fps: 0.0,
            skipped_waiting_idr_fps: 60.0,
            ..steady
        };
        assert_eq!(tracker.sample(&stalled).bottleneck, Bottleneck::WaitingForIdr);

        let display = diagnose(&Window {
            display_drop_pct: 8.0,
            ..healthy()
        });
        assert_eq!(display.bottleneck, Bottleneck::Render);

        let host = diagnose(&Window {
            host_ms: Some(45.0),
            ..healthy()
        });
        assert_eq!(host.bottleneck, Bottleneck::Host);
        assert_eq!(host.key_metric, "host 45.0 ms/frame");
    }

    #[test]
    fn worst_cause_wins() {
        let both = diagnose(&Window {
            network_drop_pct: 3.0, // 1.5
            display_drop_pct: 9.0, // 4.5
            ..healthy()
        });
        assert_eq!(both.bottleneck, Bottleneck::Render);
    }

    #[test]
    fn high_rtt_and_slow_arrivals_are_reported_even_with_client_drops() {
        let run = Window {
            target_fps: 30,
            received_fps: 9.0,
            decoded_fps: 9.0,
            rtt_ms: 1859,
            display_drop_pct: 11.0,
            ..healthy()
        };
        let verdict = diagnose(&run);
        assert_eq!(verdict.bottleneck, Bottleneck::Network);
        assert_eq!(verdict.key_metric, "RTT 1859 ms, 9 of 30 fps assembled");

        let full_rate = Window {
            received_fps: 30.0,
            decoded_fps: 30.0,
            ..run
        };
        assert_eq!(diagnose(&full_rate).bottleneck, Bottleneck::Render);
        let waiting = Window {
            received_fps: 0.0,
            decoded_fps: 0.0,
            skipped_waiting_idr_fps: 9.0,
            ..run
        };
        assert_eq!(diagnose(&waiting).bottleneck, Bottleneck::WaitingForIdr);
    }

    #[test]
    fn spike_history() {
        let mut h = SpikeHistory::new(5);
        assert_eq!(h.summarize(), None);
        for b in [
            Bottleneck::None,
            Bottleneck::Network,
            Bottleneck::Network,
            Bottleneck::Decode,
        ] {
            h.record(b);
        }
        assert_eq!(
            h.summarize().as_deref(),
            Some("3 spikes in the last ~1 min (mostly network)")
        );
        for _ in 0..5 {
            h.record(Bottleneck::None);
        }
        assert_eq!(h.summarize(), None, "old spikes roll out of the window");
    }
}
