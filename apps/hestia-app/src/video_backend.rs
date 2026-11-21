//! Composes the GUI's video decoder probe with the window's native output.

#[cfg(target_os = "android")]
use std::sync::Arc;

use hestia_core::StreamSettings;
use hestia_media::{Colorimetry, DecoderChoice};
#[cfg(target_os = "android")]
use hestia_media::gpu_video::GpuOutput;
#[cfg(target_os = "android")]
use hestia_media::video_decoder::{probe_android_decoder, select_decoder};

#[cfg(target_os = "android")]
use crate::presentation::GpuWindow;

pub(crate) struct StreamVideo {
    pub(crate) decoder: DecoderChoice,
    #[cfg(target_os = "android")]
    pub(crate) gpu: Option<Arc<GpuOutput>>,
}

impl StreamVideo {
    pub(crate) fn select(
        settings: &StreamSettings,
        #[cfg(target_os = "android")] window: &GpuWindow,
    ) -> Result<Self, String> {
        let colorimetry = Colorimetry::default();
        #[cfg(target_os = "android")]
        let gpu = settings
            .gpu_video
            .then(|| Arc::new(GpuOutput::new(window.status.clone())));
        #[cfg(target_os = "android")]
        let decoder = select_decoder(colorimetry, settings.hardware_decoding, |colorimetry| {
            let candidates = crate::platform::video_decoders(settings.width, settings.height, settings.fps)
                .unwrap_or_else(|e| {
                    log::warn!("could not check Android video decoders: {e}");
                    Vec::new()
                });
            probe_android_decoder(colorimetry, candidates, gpu.clone())
        })?;
        #[cfg(not(target_os = "android"))]
        let decoder = hestia_media::decoder_with(colorimetry, settings.hardware_decoding)?;
        Ok(Self {
            decoder,
            #[cfg(target_os = "android")]
            gpu,
        })
    }
}
