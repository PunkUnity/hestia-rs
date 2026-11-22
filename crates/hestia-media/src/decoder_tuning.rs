//! Which low-latency settings a hardware decoder gets, by its vendor.
//!
//! MediaCodec takes vendor extensions as `vendor.<extension>.<parameter>`
//! format keys. Each chipset vendor defines its own, and the rules here are
//! the ones Moonlight and Hestia (Android) have settled on over the years:
//! a vendor's keys go only to that vendor's decoders, matched by the
//! component name's prefix. A decoder that happens to accept another
//! vendor's key doesn't implement it, and some fail `configure` on keys they
//! don't know.
//!
//! Kept free of Android types so the rules are tested on any host.

/// Who made a decoder, from its component name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Qualcomm,
    Nvidia,
    Kirin,
    Exynos,
    Amlogic,
    Other,
}

const PREFIXES: &[(&str, Vendor)] = &[
    ("omx.qcom", Vendor::Qualcomm),
    ("c2.qti", Vendor::Qualcomm),
    ("c2.qcom", Vendor::Qualcomm),
    ("omx.nvidia", Vendor::Nvidia),
    ("c2.nvidia", Vendor::Nvidia),
    ("omx.hisi", Vendor::Kirin),
    ("c2.hisi", Vendor::Kirin),
    ("omx.exynos", Vendor::Exynos),
    ("c2.exynos", Vendor::Exynos),
    ("omx.amlogic", Vendor::Amlogic),
    ("c2.amlogic", Vendor::Amlogic),
];

/// The vendor of a decoder component such as `c2.qti.hevc.decoder`.
pub fn vendor(component: &str) -> Vendor {
    PREFIXES
        .iter()
        .find(|(prefix, _)| {
            component
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        })
        .map_or(Vendor::Other, |&(_, vendor)| vendor)
}

/// The vendor-specific low-latency keys for a decoder, as `(key, value)`.
///
/// Empty for a decoder of another vendor, and for one opened by MIME type
/// alone (`component` is `None`), whose vendor isn't known.
pub fn vendor_low_latency_keys(component: Option<&str>) -> Vec<(&'static str, i32)> {
    let Some(component) = component else {
        return Vec::new();
    };
    match vendor(component) {
        Vendor::Qualcomm => {
            // OMX decoders reorder unless this is 0; Codec2 ones need 1.
            let omx = component.to_ascii_lowercase().starts_with("omx.");
            vec![
                ("vendor.qti-ext-dec-picture-order.enable", i32::from(!omx)),
                ("vendor.qti-ext-dec-low-latency.enable", 1),
                // Output fences: Snapdragon 8 Gen 2 reads the first, 8s Gen 3
                // and 8 Elite the other two (1 = software fence).
                ("vendor.qti-ext-output-sw-fence-enable.value", 1),
                ("vendor.qti-ext-output-fence.enable", 1),
                ("vendor.qti-ext-output-fence.fence_type", 1),
            ]
        }
        Vendor::Nvidia => vec![
            ("media.low-latency.enable", 1),
            ("vendor.low-latency.enable", 1),
            ("disable-output-reorder", 1),
            ("vendor.nvidia.disable-output-reorder", 1),
        ],
        Vendor::Kirin => vec![
            (
                "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
                1,
            ),
            (
                "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy",
                -1,
            ),
        ],
        Vendor::Exynos => vec![("vendor.rtc-ext-dec-low-latency.enable", 1)],
        Vendor::Amlogic => vec![("vendor.low-latency.enable", 1)],
        Vendor::Other => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(component: &str) -> Vec<&'static str> {
        vendor_low_latency_keys(Some(component))
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }

    #[test]
    fn vendors_are_matched_by_prefix_ignoring_case() {
        assert_eq!(vendor("c2.qti.hevc.decoder"), Vendor::Qualcomm);
        assert_eq!(vendor("OMX.qcom.video.decoder.avc"), Vendor::Qualcomm);
        assert_eq!(vendor("c2.exynos.hevc.decoder"), Vendor::Exynos);
        assert_eq!(vendor("OMX.Nvidia.h264.decode"), Vendor::Nvidia);
        assert_eq!(vendor("c2.mtk.hevc.decoder"), Vendor::Other);
        assert_eq!(vendor("c2.android.hevc.decoder"), Vendor::Other);
        assert_eq!(vendor("c2"), Vendor::Other);
    }

    #[test]
    fn each_vendor_gets_only_its_own_keys() {
        for key in keys("c2.qti.hevc.decoder") {
            assert!(key.starts_with("vendor.qti-ext-"), "{key}");
        }
        assert_eq!(
            keys("c2.exynos.hevc.decoder"),
            ["vendor.rtc-ext-dec-low-latency.enable"]
        );
        assert_eq!(keys("omx.amlogic.hevc.decoder.awesome"), ["vendor.low-latency.enable"]);
        assert!(
            keys("c2.hisi.avc.decoder")
                .iter()
                .all(|key| key.starts_with("vendor.hisi-ext-"))
        );
        assert!(keys("c2.mtk.hevc.decoder").is_empty());
        assert!(keys("c2.google.av1.decoder").is_empty());
    }

    #[test]
    fn qualcomm_omx_decoders_turn_picture_order_off() {
        let order = |component| {
            vendor_low_latency_keys(Some(component))
                .into_iter()
                .find(|(key, _)| *key == "vendor.qti-ext-dec-picture-order.enable")
                .map(|(_, value)| value)
        };
        assert_eq!(order("OMX.qcom.video.decoder.avc"), Some(0));
        assert_eq!(order("c2.qti.avc.decoder"), Some(1));
    }

    #[test]
    fn an_unknown_decoder_gets_no_vendor_keys() {
        assert!(vendor_low_latency_keys(None).is_empty());
    }
}
