//! Selects a decoder and its advertised formats before a stream starts.

use std::sync::Arc;

#[cfg(target_os = "android")]
use crate::mediacodec::MediaCodecDecoder;
use crate::native_picture::GpuOutput;
use crate::{Colorimetry, FrameDecoder, VideoFormat};

/// Whether this build can open the PyroWave runtime. Device and driver support
/// are checked when the user explicitly selects the codec.
pub const PYROWAVE_SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    all(target_os = "android", target_pointer_width = "64")
));

/// The decoder and the formats it can announce to the host.
pub type DecoderChoice = (Box<dyn FrameDecoder>, Vec<VideoFormat>);

/// Opens the experimental Vulkan codec before launch so an unavailable
/// runtime or incompatible GPU is reported before starting the host app.
pub fn pyrowave_decoder(
    width: u32,
    height: u32,
    hardware: bool,
    gpu: Option<Arc<GpuOutput>>,
) -> Result<DecoderChoice, String> {
    if !hardware {
        return Err("Enable hardware decoding to use PyroWave".into());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        crate::pyrowave::open(width, height, gpu)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (width, height, gpu);
        Err("PyroWave is available on Linux and Android".into())
    }
}

/// Select hardware for this session when requested. The platform supplies its
/// probe, so asking for software never queries the native decoder APIs.
pub fn select_decoder(
    colorimetry: Colorimetry,
    hardware: bool,
    probe_hardware: impl FnOnce(Colorimetry) -> Option<DecoderChoice>,
) -> Result<DecoderChoice, String> {
    select_decoder_for_renderer(colorimetry, hardware, probe_hardware, None)
}

/// Like [`select_decoder`]; the software decoder hands its pictures to
/// `software_output` as 4:2:0 planes while that renderer is available
/// (the desktop's GL renderer converts them to RGB itself).
pub fn select_decoder_for_renderer(
    colorimetry: Colorimetry,
    hardware: bool,
    probe_hardware: impl FnOnce(Colorimetry) -> Option<DecoderChoice>,
    software_output: Option<Arc<GpuOutput>>,
) -> Result<DecoderChoice, String> {
    if hardware && let Some(decoder) = probe_hardware(colorimetry) {
        return Ok(decoder);
    }
    software_decoder(colorimetry, software_output)
}

/// The best video decoder this build offers without a native renderer.
pub fn default_decoder(colorimetry: Colorimetry) -> Result<DecoderChoice, String> {
    decoder_with(colorimetry, true)
}

/// Like [`default_decoder`]; `hardware: false` skips hardware decoders.
pub fn decoder_with(colorimetry: Colorimetry, hardware: bool) -> Result<DecoderChoice, String> {
    select_decoder(colorimetry, hardware, |colorimetry| {
        #[cfg(target_os = "android")]
        {
            MediaCodecDecoder::probe(colorimetry).map(mediacodec_choice)
        }
        // No window renderer here: pictures are read back as RGBA.
        #[cfg(all(feature = "vaapi", target_os = "linux"))]
        {
            crate::vaapi::probe(colorimetry, None)
        }
        #[cfg(not(any(target_os = "android", all(feature = "vaapi", target_os = "linux"))))]
        {
            let _ = colorimetry;
            None
        }
    })
}

/// Constructs the MediaCodec choice from components already matched to the
/// requested stream mode by Android's codec discovery API.
#[cfg(target_os = "android")]
pub fn probe_android_decoder(
    colorimetry: Colorimetry,
    candidates: Vec<(crate::VideoCodec, String)>,
    output: Option<Arc<GpuOutput>>,
    balanced_pacing: bool,
) -> Option<DecoderChoice> {
    MediaCodecDecoder::with_components(colorimetry, candidates, output)
        .map(|decoder| mediacodec_choice(decoder.with_balanced_pacing(balanced_pacing)))
}

#[cfg(target_os = "android")]
pub fn decoder_with_android_capabilities(
    colorimetry: Colorimetry,
    candidates: Vec<(crate::VideoCodec, String)>,
    output: Option<Arc<GpuOutput>>,
) -> Result<DecoderChoice, String> {
    select_decoder(colorimetry, true, |colorimetry| {
        probe_android_decoder(colorimetry, candidates, output, false)
    })
}

#[cfg(target_os = "android")]
fn mediacodec_choice(decoder: MediaCodecDecoder) -> DecoderChoice {
    let formats = decoder.formats();
    (Box::new(decoder), formats)
}

fn software_decoder(colorimetry: Colorimetry, output: Option<Arc<GpuOutput>>) -> Result<DecoderChoice, String> {
    #[cfg(feature = "openh264")]
    {
        use crate::software_h264::OpenH264Decoder;

        let decoder = OpenH264Decoder::with_gpu_output(colorimetry, output)?;
        Ok((Box::new(decoder), OpenH264Decoder::formats()))
    }
    #[cfg(not(feature = "openh264"))]
    {
        let _ = (colorimetry, output);
        Err("this build has no video decoder (enable the `openh264` feature)".into())
    }
}

#[cfg(all(test, feature = "openh264"))]
mod tests {
    use super::*;

    #[test]
    fn software_selection_skips_the_probe_and_unavailable_hardware_falls_back() {
        let (_, forced) =
            select_decoder(Colorimetry::default(), false, |_| panic!("unexpected hardware probe")).unwrap();
        let (_, fallback) = select_decoder(Colorimetry::default(), true, |_| None).unwrap();
        assert_eq!(forced, crate::software_h264::OpenH264Decoder::formats());
        assert_eq!(fallback, forced);
    }

    #[test]
    fn selected_backend_supplies_its_advertised_formats() {
        let (decoder, _) = software_decoder(Colorimetry::default(), None).unwrap();
        let format = VideoFormat {
            codec: crate::VideoCodec::Av1,
            ten_bit: false,
            yuv444: false,
        };
        let (_, advertised) = select_decoder(Colorimetry::default(), true, |_| Some((decoder, vec![format]))).unwrap();
        assert_eq!(advertised, [format]);
    }
}
