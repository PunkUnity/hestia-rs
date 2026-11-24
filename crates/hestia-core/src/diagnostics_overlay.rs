//! Device preferences for which measurements appear in the live overlay.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub enum OverlayMetric {
    Resolution,
    FrameRate,
    Bitrate,
    NetworkLatency,
    DecodeTime,
    HostTime,
    RecoveredPackets,
    LostFrames,
    KeyframeSkips,
    ClientDrops,
    Warnings,
    SpikeHistory,
}

impl OverlayMetric {
    pub const ALL: [Self; 12] = [
        Self::Resolution,
        Self::FrameRate,
        Self::Bitrate,
        Self::NetworkLatency,
        Self::DecodeTime,
        Self::HostTime,
        Self::RecoveredPackets,
        Self::LostFrames,
        Self::KeyframeSkips,
        Self::ClientDrops,
        Self::Warnings,
        Self::SpikeHistory,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Resolution => "resolution",
            Self::FrameRate => "frame_rate",
            Self::Bitrate => "bitrate",
            Self::NetworkLatency => "network_latency",
            Self::DecodeTime => "decode_time",
            Self::HostTime => "host_time",
            Self::RecoveredPackets => "recovered_packets",
            Self::LostFrames => "lost_frames",
            Self::KeyframeSkips => "keyframe_skips",
            Self::ClientDrops => "client_drops",
            Self::Warnings => "warnings",
            Self::SpikeHistory => "spike_history",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Resolution => "Resolution",
            Self::FrameRate => "Frame rate (FPS)",
            Self::Bitrate => "Bitrate",
            Self::NetworkLatency => "Network round-trip time (RTT)",
            Self::DecodeTime => "Client decode time",
            Self::HostTime => "Host processing time",
            Self::RecoveredPackets => "Packets recovered (FEC)",
            Self::LostFrames => "Frames lost during assembly",
            Self::KeyframeSkips => "Frames skipped waiting for a keyframe",
            Self::ClientDrops => "Frames dropped on the client",
            Self::Warnings => "Current diagnostic warning",
            Self::SpikeHistory => "Recent spike summary",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiagnosticsOverlay {
    // Store the hidden metrics so existing installs and new metrics default to visible.
    hidden: BTreeSet<String>,
}

impl DiagnosticsOverlay {
    pub fn shows(&self, metric: OverlayMetric) -> bool {
        !self.hidden.contains(metric.id())
    }

    pub fn set_visible(&mut self, metric: OverlayMetric, visible: bool) {
        if visible {
            self.hidden.remove(metric.id());
        } else {
            self.hidden.insert(metric.id().to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_preferences_default_to_visible_and_preserve_saved_choices() {
        let mut preferences: DiagnosticsOverlay = serde_json::from_str("{}").unwrap();
        assert!(OverlayMetric::ALL.into_iter().all(|metric| preferences.shows(metric)));
        preferences.set_visible(OverlayMetric::NetworkLatency, false);

        let saved = serde_json::to_string(&preferences).unwrap();
        assert_eq!(saved, r#"{"hidden":["network_latency"]}"#);
        let restored: DiagnosticsOverlay = serde_json::from_str(&saved).unwrap();
        assert!(!restored.shows(OverlayMetric::NetworkLatency));
        assert!(restored.shows(OverlayMetric::FrameRate));
    }
}
