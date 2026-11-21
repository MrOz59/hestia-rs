//! Composes the GUI's video decoder probe with the window's native output.
//!
//! The "GPU video" setting (`gpu_video`) gives the stream a [`GpuOutput`]
//! tied to the window's renderer: on Android, MediaCodec renders into
//! hardware buffers drawn without a CPU copy; on the desktop, OpenH264 hands
//! YUV planes to the GL pass that converts them to RGB. Either way the
//! decoder falls back to RGBA on the CPU whenever that renderer is not
//! available.

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
                probe_android_decoder(colorimetry, candidates, gpu.clone())
            },
            // Android's renderer draws hardware buffers only: OpenH264
            // (the fallback) converts on the CPU.
            None,
        )?;
        // No hardware decoder on the desktop yet (VA-API is planned): OpenH264
        // with GPU colour conversion.
        #[cfg(not(target_os = "android"))]
        let decoder = select_decoder_for_renderer(colorimetry, settings.hardware_decoding, |_| None, gpu.clone())?;
        Ok(Self { decoder, gpu })
    }
}
