//! Desktop GPU video: draws OpenH264's YUV planes, or a hardware decoder's
//! DMA-BUF pictures, into the texture the stream view shows, converting to
//! RGB on the GPU (`hestia_frontend::gl_yuv` and `gl_dmabuf` do the GL and
//! EGL work; this is the Slint glue).
//!
//! Runs inside Slint's rendering notifier, with its OpenGL context current
//! (the default FemtoVG renderer on winit; Skia on OpenGL works too). Each
//! new picture is uploaded into persistent textures and drawn into one RGBA
//! texture that the stream view keeps showing through a borrowed-texture
//! image, so a frame costs neither a CPU colour conversion nor an RGBA copy
//! and texture allocation in Slint. DMA-BUF pictures (VA-API, Linux) are
//! imported as EGL images instead of uploaded: no copy at all.
//!
//! When the renderer is not OpenGL (the software renderer, Qt) or anything
//! here fails, the status says so and the decoder goes back to RGBA output
//! on the CPU; the stream carries on. Without EGL DMA-BUF import (X11/GLX,
//! or an import failing), only that part goes: hardware-decoded pictures
//! are copied into planes instead.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[cfg(target_os = "linux")]
use hestia_frontend::gl_dmabuf::DmabufImporter;
#[cfg(target_os = "linux")]
use hestia_frontend::gl_yuv::Nv12Images;
use hestia_frontend::gl_yuv::{Planes, YuvPass};
use hestia_frontend::presentation::GpuWindow;
use hestia_media::native_picture::NativePicture;
use hestia_media::{DecodedFrame, FrameBuffer};
use log::{info, warn};
use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;

struct Renderer {
    weak: slint::Weak<AppWindow>,
    gpu: Arc<GpuWindow>,
    pass: Option<YuvPass>,
    /// Imports DMA-BUF pictures, when the window renders through EGL.
    #[cfg(target_os = "linux")]
    dmabuf: Option<DmabufImporter>,
    /// The last DMA-BUF picture drawn, kept until the next draw: its buffer
    /// goes back to the decoder only once the GPU has surely read it.
    held: Option<DecodedFrame>,
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
                #[cfg(target_os = "linux")]
                self.setup_dmabuf(&pass);
                self.pass = Some(pass);
                self.gpu.status.ready();
            }
            Err(e) => self.gpu.status.fail(e),
        }
    }

    /// Hardware-decoded pictures can be drawn without a copy when the
    /// window's EGL display imports DMA-BUFs.
    #[cfg(target_os = "linux")]
    fn setup_dmabuf(&mut self, pass: &YuvPass) {
        if !pass.draws_egl_images() {
            info!("DMA-BUF video import unavailable (no GL_OES_EGL_image): hardware-decoded pictures are copied");
            return;
        }
        match DmabufImporter::new() {
            Ok(importer) => {
                match &importer.render_node {
                    Some(node) => info!("DMA-BUF video import ready (GPU {})", node.display()),
                    None => info!("DMA-BUF video import ready"),
                }
                self.gpu.status.set_dmabuf(importer.render_node.clone());
                self.dmabuf = Some(importer);
            }
            Err(e) => info!("DMA-BUF video import unavailable ({e}): hardware-decoded pictures are copied"),
        }
    }

    /// Converts the frame's picture into the pass's target texture: its
    /// size, the texture and whether it was reallocated. `Err` with `true`
    /// when only DMA-BUF import failed (the rest of the GPU path works).
    fn draw(&mut self, frame: &DecodedFrame) -> Option<Result<((u32, u32), (u32, bool)), (String, bool)>> {
        let pass = self.pass.as_mut()?;
        match &frame.buffer {
            FrameBuffer::Native(NativePicture::Planar(picture)) => {
                let (y, u, v) = picture.planes();
                let (matrix, offset) = picture.colorimetry.gpu_conversion();
                let size = (picture.width, picture.height);
                let planes = Planes {
                    y,
                    u,
                    v,
                    width: size.0,
                    height: size.1,
                    matrix,
                    offset,
                };
                Some(pass.draw(&planes).map(|drawn| (size, drawn)).map_err(|e| (e, false)))
            }
            #[cfg(target_os = "linux")]
            FrameBuffer::Native(NativePicture::Dmabuf(picture)) => {
                let Some(importer) = self.dmabuf.as_ref() else {
                    return Some(Err(("no DMA-BUF importer".into(), true)));
                };
                let size = (picture.width, picture.height);
                let drawn = importer.import(picture).and_then(|imported| {
                    let (matrix, offset) = picture.colorimetry.gpu_conversion();
                    pass.draw_nv12(&Nv12Images {
                        images: imported.images,
                        width: size.0,
                        height: size.1,
                        scale: [
                            size.0 as f32 / picture.coded_width.max(1) as f32,
                            size.1 as f32 / picture.coded_height.max(1) as f32,
                        ],
                        matrix,
                        offset,
                    })
                });
                Some(drawn.map(|drawn| (size, drawn)).map_err(|e| (e, true)))
            }
            _ => None,
        }
    }

    fn before_rendering(&mut self) {
        if self.pass.is_none() {
            return;
        }
        let Some(presentation) = self.gpu.active() else { return };
        let Some(frame) = presentation.take_gpu() else { return };
        let Some(result) = self.draw(&frame) else { return };
        match result {
            Ok((size, (texture, resized))) => {
                let now = Instant::now();
                let stats = &presentation.gpu;
                stats.ready_to_draw.record(now.duration_since(frame.decoded_at));
                stats.receive_to_draw.record(now.duration_since(frame.received_at));
                stats.recovery.record(frame.idr_requested_at, now);
                stats.drawn.fetch_add(1, Ordering::Relaxed);
                if matches!(frame.buffer, FrameBuffer::Native(NativePicture::Planar(_))) {
                    // The upload copied the planes: give them back to the decoder.
                    drop(frame);
                } else {
                    // Sampled in place: the previous one is free now.
                    self.held = Some(frame);
                }
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
            #[cfg(target_os = "linux")]
            Err((e, true)) => {
                // Only the import: decoders copy their pictures from now on.
                self.gpu.status.dmabuf_failed(e);
                self.dmabuf = None;
                presentation.discard_gpu();
            }
            Err((e, _)) => {
                if let Some(pass) = &self.pass {
                    pass.finish();
                }
                self.gpu.status.fail(e);
                presentation.discard_gpu();
                self.held = None;
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
        self.held = None;
        #[cfg(target_os = "linux")]
        {
            self.dmabuf = None;
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
        #[cfg(target_os = "linux")]
        dmabuf: None,
        held: None,
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
