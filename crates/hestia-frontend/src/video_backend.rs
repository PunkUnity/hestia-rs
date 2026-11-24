//! Composes the GUI's video decoder probe with the window's native output.
//!
//! The "GPU video" setting (`gpu_video`) gives the stream a [`GpuOutput`]
//! tied to the window's renderer: on Android, MediaCodec renders into
//! hardware buffers drawn without a CPU copy; on Linux, VA-API decodes into
//! DMA-BUFs the GL pass draws in place, and OpenH264 hands it YUV planes.
//! Either way the decoder falls back to RGBA on the CPU whenever that
//! renderer is not available.

use std::sync::Arc;

use hestia_core::StreamSettings;
use hestia_media::native_picture::GpuOutput;
#[cfg(target_os = "android")]
use hestia_media::video_decoder::probe_android_decoder;
use hestia_media::video_decoder::select_decoder_for_renderer;
use hestia_media::{Colorimetry, DecoderChoice};

use crate::presentation::GpuWindow;

pub(crate) struct StreamVideo {
    pub(crate) decoder: DecoderChoice,
    pub(crate) gpu: Option<Arc<GpuOutput>>,
}

impl StreamVideo {
    pub(crate) fn select(settings: &StreamSettings, window: &GpuWindow) -> Result<Self, String> {
        let colorimetry = Colorimetry::default();
        let gpu = settings
            .gpu_video
            .then(|| Arc::new(GpuOutput::new(window.status.clone())));
        if settings.codec == hestia_core::CodecPreference::PyroWave {
            if settings.hdr {
                return Err("PyroWave requires SDR. Turn off HDR in stream settings.".into());
            }
            let decoder = hestia_media::video_decoder::pyrowave_decoder(
                settings.width,
                settings.height,
                settings.hardware_decoding,
                gpu.clone(),
            )?;
            return Ok(Self { decoder, gpu });
        }
        #[cfg(target_os = "android")]
        let decoder = select_decoder_for_renderer(
            colorimetry,
            settings.hardware_decoding,
            |colorimetry| {
                let candidates = crate::platform::video_decoders(settings.width, settings.height, settings.fps)
                    .unwrap_or_else(|e| {
                        log::warn!("could not check Android video decoders: {e}");
                        Vec::new()
                    });
                probe_android_decoder(
                    colorimetry,
                    candidates,
                    gpu.clone(),
                    settings.frame_pacing == hestia_core::FramePacing::Balanced,
                )
            },
            // Android's renderer draws hardware buffers only: OpenH264
            // (the fallback) converts on the CPU.
            None,
        )?;
        // VA-API on Linux (on the renderer's GPU when it knows it), else
        // OpenH264 with GPU colour conversion.
        #[cfg(not(target_os = "android"))]
        let decoder = select_decoder_for_renderer(
            colorimetry,
            settings.hardware_decoding,
            |colorimetry| {
                #[cfg(all(feature = "vaapi", target_os = "linux"))]
                {
                    hestia_media::vaapi::probe(colorimetry, gpu.clone())
                }
                #[cfg(not(all(feature = "vaapi", target_os = "linux")))]
                {
                    let _ = colorimetry;
                    None
                }
            },
            gpu.clone(),
        )?;
        Ok(Self { decoder, gpu })
    }
}
