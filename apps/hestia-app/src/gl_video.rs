//! Desktop GPU video: draws OpenH264's YUV planes into the texture the
//! stream view shows, converting to RGB on the GPU (see `gl_yuv`).
//!
//! Runs inside Slint's rendering notifier, with its OpenGL context current
//! (the default FemtoVG renderer on winit; Skia on OpenGL works too). Each
//! new picture is uploaded into persistent textures and drawn into one RGBA
//! texture that the stream view keeps showing through a borrowed-texture
//! image, so a frame costs neither a CPU colour conversion nor an RGBA copy
//! and texture allocation in Slint.
//!
//! When the renderer is not OpenGL (the software renderer, Qt) or anything
//! here fails, the status says so and the decoder goes back to RGBA output
//! on the CPU; the stream carries on.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use hestia_media::FrameBuffer;
use hestia_media::native_picture::NativePicture;
use log::{info, warn};
use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;
use crate::gl_yuv::{Planes, YuvPass};
use crate::presentation::GpuWindow;

struct Renderer {
    weak: slint::Weak<AppWindow>,
    gpu: Arc<GpuWindow>,
    pass: Option<YuvPass>,
    /// The stream view shows our texture (for this stream, at this size).
    image_set: Option<(u64, (u32, u32))>,
}

impl Renderer {
    fn setup(&mut self, api: &GraphicsAPI<'_>) {
        let GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
            self.gpu
                .status
                .fail("the renderer does not use OpenGL; video colour is converted on the CPU".into());
            return;
        };
        match YuvPass::new(*get_proc_address) {
            Ok(pass) => {
                info!("GPU video colour conversion ready ({})", pass.version);
                self.pass = Some(pass);
                self.gpu.status.ready();
            }
            Err(e) => self.gpu.status.fail(e),
        }
    }

    fn before_rendering(&mut self) {
        let Some(pass) = self.pass.as_mut() else { return };
        let Some(presentation) = self.gpu.active() else { return };
        let Some(frame) = presentation.take_gpu() else { return };
        let FrameBuffer::Native(NativePicture::Planar(picture)) = &frame.buffer else {
            return;
        };
        let (y, u, v) = picture.planes();
        let (matrix, offset) = picture.colorimetry.gpu_conversion();
        let size = (picture.width, picture.height);
        let result = pass.draw(&Planes {
            y,
            u,
            v,
            width: size.0,
            height: size.1,
            matrix,
            offset,
        });
        match result {
            Ok((texture, resized)) => {
                let now = Instant::now();
                let stats = &presentation.gpu;
                stats.ready_to_draw.record(now.duration_since(frame.decoded_at));
                stats.receive_to_draw.record(now.duration_since(frame.received_at));
                stats.recovery.record(frame.idr_requested_at, now);
                stats.drawn.fetch_add(1, Ordering::Relaxed);
                // The upload copied the planes: give them back to the decoder.
                drop(frame);
                if !presentation.gpu_available() || !self.gpu.is_active(presentation.generation) {
                    return;
                }
                let key = (presentation.generation, size);
                if resized || self.image_set != Some(key) {
                    let Some(id) = NonZeroU32::new(texture) else { return };
                    // SAFETY: a live RGBA texture of this context, of that
                    // size, kept until the pass is deleted at teardown.
                    let image =
                        unsafe { slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(id, size.into()) }.build();
                    if let Some(ui) = self.weak.upgrade() {
                        ui.set_stream_frame(image);
                        ui.set_stream_status(slint::SharedString::new());
                    }
                    self.image_set = Some(key);
                }
            }
            Err(e) => {
                pass.finish();
                self.gpu.status.fail(e);
                presentation.discard_gpu();
                self.clear_image();
                if let Some(mut pass) = self.pass.take() {
                    pass.delete();
                }
            }
        }
    }

    fn clear_image(&mut self) {
        if self.image_set.take().is_some()
            && let Some(ui) = self.weak.upgrade()
        {
            ui.set_stream_frame(slint::Image::default());
        }
    }

    fn teardown(&mut self) {
        self.gpu.status.pause();
        if let Some(presentation) = self.gpu.active() {
            presentation.discard_gpu();
        }
        self.clear_image();
        if let Some(mut pass) = self.pass.take() {
            pass.delete();
        }
    }
}

/// Hooks the GPU colour conversion into the window's rendering (desktop).
pub(crate) fn install(ui: &AppWindow, gpu: Arc<GpuWindow>) {
    let mut renderer = Renderer {
        weak: ui.as_weak(),
        gpu,
        pass: None,
        image_set: None,
    };
    let result = ui.window().set_rendering_notifier(move |state, api| match state {
        RenderingState::RenderingSetup => renderer.setup(api),
        RenderingState::BeforeRendering => renderer.before_rendering(),
        RenderingState::RenderingTeardown => renderer.teardown(),
        _ => {}
    });
    if let Err(e) = result {
        warn!("GPU video colour conversion unavailable: {e:?}");
    }
}
