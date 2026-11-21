//! Hardware video decoding on Linux through VA-API (feature `vaapi`).
//!
//! Bitstream parsing and reference handling come from cros-codecs (ChromeOS'
//! stateless decoders, vendored in `third_party/cros-codecs` with a
//! low-delay mode); the GPU decodes through libva. H.264, HEVC and AV1,
//! 8-bit 4:2:0.
//!
//! **Memory.** The driver allocates NV12 surfaces for the stream's coded
//! size, and each one is exported once as a DMA-BUF. cros-codecs decodes
//! into frames it gets from us, which import those DMA-BUFs back as VA
//! surfaces, so every picture lives in memory that both libva and the
//! window's renderer can reach. A frame returns to the pool when the decoder
//! no longer needs it as a reference *and* the renderer has let it go.
//!
//! **Output**, chosen per picture like the software decoder does:
//! * **zero-copy** (the window's renderer imports DMA-BUFs, Linux EGL): the
//!   picture goes to the renderer as a [`DmabufPicture`] and the GPU converts
//!   it to RGB while drawing;
//! * **GPU colour**: the renderer draws planes but cannot import (X11/GLX,
//!   an import failure): the picture is read back and copied as I420;
//! * **CPU colour**: no GPU renderer (CLI, software renderer): read back and
//!   converted to RGBA.
//!
//! **Latency.** Each access unit is fed whole and finished at once, and
//! pictures leave in decoding order as soon as they are decoded (hosts send
//! no B-frames), so a picture comes out of the call that fed it.
//!
//! **Threads.** libva objects here are reference counted with `Rc`. The
//! decoding session is created on the first decode, on the decoder thread,
//! and never leaves it (see the `Send` impl).

use std::any::Any;
use std::os::fd::{AsRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::ThreadId;
use std::time::Instant;

use cros_codecs::backend::vaapi::decoder::VaapiBackend;
use cros_codecs::decoder::stateless::av1::Av1;
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::h265::H265;
use cros_codecs::decoder::stateless::{DecodeError, StatelessDecoder, StatelessVideoDecoder};
use cros_codecs::decoder::{DecodedHandle, DecoderEvent};
use cros_codecs::libva::{
    self, Display, ExternalBufferDescriptor, MemoryType, Surface, UsageHint, VADRMPRIMESurfaceDescriptor,
    VADRMPRIMESurfaceDescriptorLayer, VADRMPRIMESurfaceDescriptorObject,
};
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::{BlockingMode, Fourcc, Resolution};
use hestia_stream::{EncodedFrame, VideoCodec, VideoFormat};
use log::{info, warn};

use crate::color::{Colorimetry, Yuv420, yuv420_to_rgba};
use crate::decode::{DecodedFrame, FrameBuffer, FrameDecoder, FrameTiming};
use crate::native_picture::{DmabufPicture, DmabufPlane, GpuOutput, NativePicture, PlanarPicture, PlanePool};
use crate::video_decoder::DecoderChoice;

/// `DRM_FORMAT_NV12` (the same code as `VA_FOURCC_NV12`).
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// Frames beyond the codec's minimum: one being decoded into, one waiting
/// for the renderer, one being drawn, one the renderer keeps until the next
/// draw has replaced it.
const EXTRA_FRAMES: usize = 4;

/// How often one access unit may stop for decoder events (a format change,
/// a resolution change flush) before the decoder counts as stuck.
const MAX_EVENT_WAITS: usize = 4;

/// Identities for [`DmabufPicture::buffer_id`].
static NEXT_BUFFER_ID: AtomicU64 = AtomicU64::new(1);

/// The renderer's GPU first (DMA-BUFs import only within one GPU), then
/// every render node.
fn render_nodes(preferred: Option<PathBuf>) -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    if let Some(preferred) = preferred {
        nodes.retain(|node| *node != preferred);
        nodes.insert(0, preferred);
    }
    nodes
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// Where a plane lies in an exported surface.
#[derive(Debug, Clone, Copy)]
struct PlaneLayout {
    object: usize,
    offset: u32,
    pitch: u32,
}

/// A driver-allocated NV12 surface, exported as DMA-BUFs.
#[derive(Debug)]
struct FrameMemory {
    /// The surface it was exported from, in `Session::surfaces`.
    index: usize,
    /// The session allocation it belongs to.
    generation: u64,
    buffer_id: u64,
    /// DMA-BUF, size and format modifier of each memory object.
    objects: Vec<(OwnedFd, u32, u64)>,
    /// Y and UV, then their compression planes if the driver compresses.
    planes: Vec<PlaneLayout>,
    /// The surface's size (the coded size).
    coded: Resolution,
}

impl FrameMemory {
    fn export(surface: &Surface<()>, index: usize, generation: u64) -> Result<Self, String> {
        let desc = surface
            .export_prime()
            .map_err(|e| format!("exporting a surface as DMA-BUF: {e}"))?;
        let [layer] = &desc.layers[..] else {
            return Err(format!(
                "exported surface has {} layers, expected one",
                desc.layers.len()
            ));
        };
        let num_planes = layer.num_planes as usize;
        if layer.drm_format != DRM_FORMAT_NV12 || !matches!(num_planes, 2 | 4) {
            return Err(format!(
                "exported surface is {:?} with {num_planes} planes, expected NV12",
                Fourcc::from(layer.drm_format)
            ));
        }
        let planes: Vec<PlaneLayout> = (0..num_planes)
            .map(|i| PlaneLayout {
                object: usize::from(layer.object_index[i]),
                offset: layer.offset[i],
                pitch: layer.pitch[i],
            })
            .collect();
        if planes.iter().any(|p| p.object >= desc.objects.len()) || desc.objects.len() > 4 {
            return Err("exported surface names a missing memory object".into());
        }
        Ok(Self {
            index,
            generation,
            buffer_id: NEXT_BUFFER_ID.fetch_add(1, Ordering::Relaxed),
            objects: desc
                .objects
                .into_iter()
                .map(|o| (o.fd, o.size, o.drm_format_modifier))
                .collect(),
            planes,
            coded: Resolution {
                width: desc.width,
                height: desc.height,
            },
        })
    }

    /// A new VA surface over this memory (what cros-codecs decodes into).
    fn import(self: &Arc<Self>, display: &Rc<Display>) -> Result<Surface<VaImport>, String> {
        let fds = self
            .objects
            .iter()
            .map(|(fd, _, _)| fd.try_clone())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("duplicating a DMA-BUF: {e}"))?;
        let import = VaImport {
            fds,
            memory: self.clone(),
        };
        let mut surfaces = display
            .create_surfaces(
                libva::VA_RT_FORMAT_YUV420,
                Some(libva::VA_FOURCC_NV12),
                self.coded.width,
                self.coded.height,
                Some(UsageHint::USAGE_HINT_DECODER),
                vec![import],
            )
            .map_err(|e| format!("importing a DMA-BUF as a surface: {e}"))?;
        surfaces
            .pop()
            .ok_or_else(|| "importing a DMA-BUF made no surface".into())
    }
}

/// The DRM PRIME descriptor libva imports a [`FrameMemory`] from.
struct VaImport {
    /// Duplicates of the memory's DMA-BUFs, owned by the surface.
    fds: Vec<OwnedFd>,
    memory: Arc<FrameMemory>,
}

impl ExternalBufferDescriptor for VaImport {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> VADRMPRIMESurfaceDescriptor {
        let memory = &self.memory;
        let mut objects: [VADRMPRIMESurfaceDescriptorObject; 4] = Default::default();
        for (object, (fd, (_, size, modifier))) in objects.iter_mut().zip(self.fds.iter().zip(&memory.objects)) {
            *object = VADRMPRIMESurfaceDescriptorObject {
                fd: fd.as_raw_fd(),
                size: *size,
                drm_format_modifier: *modifier,
            };
        }
        let mut layer = VADRMPRIMESurfaceDescriptorLayer {
            drm_format: DRM_FORMAT_NV12,
            num_planes: memory.planes.len() as u32,
            object_index: [0; 4],
            offset: [0; 4],
            pitch: [0; 4],
        };
        for (i, plane) in memory.planes.iter().enumerate() {
            layer.object_index[i] = plane.object as u32;
            layer.offset[i] = plane.offset;
            layer.pitch[i] = plane.pitch;
        }
        VADRMPRIMESurfaceDescriptor {
            fourcc: libva::VA_FOURCC_NV12,
            width: memory.coded.width,
            height: memory.coded.height,
            num_objects: self.fds.len() as u32,
            objects,
            num_layers: 1,
            layers: [layer, Default::default(), Default::default(), Default::default()],
        }
    }
}

/// Free frame memory of one allocation.
#[derive(Debug, Default)]
struct FramePool {
    free: Mutex<Vec<Arc<FrameMemory>>>,
}

impl FramePool {
    fn put(&self, memory: Arc<FrameMemory>) {
        self.free.lock().unwrap_or_else(|p| p.into_inner()).push(memory);
    }

    fn take(self: &Arc<Self>, visible: Resolution) -> Option<VaFrame> {
        let memory = self.free.lock().unwrap_or_else(|p| p.into_inner()).pop()?;
        Some(VaFrame {
            memory,
            visible,
            pool: Arc::downgrade(self),
        })
    }
}

/// A frame cros-codecs decodes into. Dropping it (when neither the decoder
/// nor the renderer holds it any more) returns its memory to the pool.
#[derive(Debug)]
pub struct VaFrame {
    memory: Arc<FrameMemory>,
    visible: Resolution,
    pool: Weak<FramePool>,
}

impl Drop for VaFrame {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.put(self.memory.clone());
        }
    }
}

impl VaFrame {
    fn dmabuf(self: &Arc<Self>, colorimetry: Colorimetry) -> DmabufPicture {
        let memory = &self.memory;
        let planes = memory
            .planes
            .iter()
            .map(|p| {
                let (fd, _, modifier) = &memory.objects[p.object];
                DmabufPlane {
                    fd: fd.as_raw_fd(),
                    offset: p.offset,
                    pitch: p.pitch,
                    modifier: *modifier,
                }
            })
            .collect();
        DmabufPicture::new(
            self.clone(),
            planes,
            (memory.coded.width, memory.coded.height),
            (self.visible.width, self.visible.height),
            colorimetry,
            memory.buffer_id,
        )
    }
}

impl VideoFrame for VaFrame {
    type MemDescriptor = VaImport;
    type NativeHandle = Surface<VaImport>;

    fn fourcc(&self) -> Fourcc {
        Fourcc::from(DRM_FORMAT_NV12)
    }

    fn resolution(&self) -> Resolution {
        self.visible
    }

    fn get_plane_size(&self) -> Vec<usize> {
        let rows = [self.memory.coded.height, self.memory.coded.height.div_ceil(2)];
        self.memory.planes[..2]
            .iter()
            .zip(rows)
            .map(|(p, rows)| p.pitch as usize * rows as usize)
            .collect()
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        self.memory.planes[..2].iter().map(|p| p.pitch as usize).collect()
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("VA-API frames are read back through their surface".into())
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("VA-API frames are written by the decoder only".into())
    }

    fn to_native_handle(&self, display: &Rc<Display>) -> Result<Self::NativeHandle, String> {
        self.memory.import(display)
    }
}

/// What one VA-API device decodes.
struct Capabilities {
    vendor: String,
    codecs: Vec<VideoCodec>,
}

impl Capabilities {
    fn query(node: &Path) -> Result<Self, String> {
        let display = Display::open_drm_display(node).map_err(|e| e.to_string())?;
        let vendor = display
            .query_vendor_string()
            .unwrap_or_else(|_| "unknown driver".to_owned());
        let profiles = display
            .query_config_profiles()
            .map_err(|e| format!("listing profiles: {e}"))?;
        let decodes = |profile| {
            profiles.contains(&profile)
                && display
                    .query_config_entrypoints(profile)
                    .is_ok_and(|e| e.contains(&libva::VAEntrypoint::VAEntrypointVLD))
        };
        // cros-codecs starts every decoder with an H.264 Main configuration.
        if !decodes(libva::VAProfile::VAProfileH264Main) {
            return Err(format!("{vendor}: no H.264 decoding"));
        }
        if !decodes(libva::VAProfile::VAProfileH264High) {
            warn!("VA-API ({vendor}): no H.264 High profile; High-profile streams will fail");
        }
        let mut codecs = vec![VideoCodec::H264];
        if decodes(libva::VAProfile::VAProfileHEVCMain) {
            codecs.push(VideoCodec::Hevc);
        }
        if decodes(libva::VAProfile::VAProfileAV1Profile0) {
            codecs.push(VideoCodec::Av1);
        }
        // Decoding into our frames needs a surface to survive a DMA-BUF
        // export and import (NVIDIA's VA-API driver, for one, cannot import).
        let surfaces = display
            .create_surfaces::<()>(
                libva::VA_RT_FORMAT_YUV420,
                Some(libva::VA_FOURCC_NV12),
                64,
                64,
                Some(UsageHint::USAGE_HINT_DECODER),
                vec![()],
            )
            .map_err(|e| format!("{vendor}: creating a surface: {e}"))?;
        let memory = Arc::new(FrameMemory::export(&surfaces[0], 0, 0).map_err(|e| format!("{vendor}: {e}"))?);
        memory.import(&display).map_err(|e| format!("{vendor}: {e}"))?;
        Ok(Self { vendor, codecs })
    }
}

/// Opens VA-API on the window renderer's GPU, or the first render node that
/// works, and checks which codecs it decodes and that decoding into shared
/// memory works there. `None` when nothing does: the caller uses software.
pub fn probe(colorimetry: Colorimetry, output: Option<Arc<GpuOutput>>) -> Option<DecoderChoice> {
    let preferred = output.as_ref().and_then(|o| o.render_node());
    for node in render_nodes(preferred) {
        match catch_unwind(|| Capabilities::query(&node)) {
            Ok(Ok(caps)) => {
                info!(
                    "VA-API on {}: {} decodes {:?}",
                    node.display(),
                    caps.vendor,
                    caps.codecs
                );
                let decoder = VaapiDecoder::new(node, caps, colorimetry, output);
                let formats = decoder.formats();
                return Some((Box::new(decoder), formats));
            }
            Ok(Err(e)) => info!("VA-API on {}: {e}", node.display()),
            Err(panic) => warn!(
                "VA-API on {}: probing panicked: {}",
                node.display(),
                panic_message(panic.as_ref())
            ),
        }
    }
    None
}

/// The codec-specific cros-codecs decoder.
enum Codec {
    H264(StatelessDecoder<H264, VaapiBackend<VaFrame>>),
    H265(StatelessDecoder<H265, VaapiBackend<VaFrame>>),
    Av1(StatelessDecoder<Av1, VaapiBackend<VaFrame>>),
}

macro_rules! with_decoder {
    ($codec:expr, $decoder:ident => $body:expr) => {
        match $codec {
            Codec::H264($decoder) => $body,
            Codec::H265($decoder) => $body,
            Codec::Av1($decoder) => $body,
        }
    };
}

/// A decoded picture, ready.
struct Output {
    frame: Arc<VaFrame>,
}

enum Event {
    Frame(Output),
    FormatChanged,
}

/// Everything that holds libva objects; lives on the decoder thread.
struct Session {
    thread: ThreadId,
    display: Rc<Display>,
    codec: Codec,
    pool: Arc<FramePool>,
    /// The allocation the pool's memory was exported from; kept for reading
    /// pictures back.
    surfaces: Vec<Surface<()>>,
    generation: u64,
    /// Coded size and frame count of the current allocation.
    allocation: Option<(Resolution, usize)>,
    visible: Resolution,
    nv12: Option<libva::VAImageFormat>,
    ready: Vec<Output>,
}

impl Session {
    fn open(node: &Path, codec: VideoCodec) -> Result<Self, String> {
        let display = Display::open_drm_display(node).map_err(|e| e.to_string())?;
        let blocking = BlockingMode::Blocking;
        let codec = match codec {
            VideoCodec::H264 => {
                let mut decoder = StatelessDecoder::<H264, VaapiBackend<VaFrame>>::new_vaapi(display.clone(), blocking)
                    .map_err(|e| e.to_string())?;
                decoder.set_low_delay(true);
                Codec::H264(decoder)
            }
            VideoCodec::Hevc => {
                let mut decoder = StatelessDecoder::<H265, VaapiBackend<VaFrame>>::new_vaapi(display.clone(), blocking)
                    .map_err(|e| e.to_string())?;
                decoder.set_low_delay(true);
                Codec::H265(decoder)
            }
            VideoCodec::Av1 => Codec::Av1(
                StatelessDecoder::<Av1, VaapiBackend<VaFrame>>::new_vaapi(display.clone(), blocking)
                    .map_err(|e| e.to_string())?,
            ),
        };
        Ok(Self {
            thread: std::thread::current().id(),
            display,
            codec,
            pool: Arc::default(),
            surfaces: Vec::new(),
            generation: 0,
            allocation: None,
            visible: Resolution::default(),
            nv12: None,
            ready: Vec::new(),
        })
    }

    /// Feeds one access unit; returns the newest picture it completed.
    fn feed(&mut self, timestamp: u64, data: &[u8]) -> Result<Option<Output>, String> {
        let mut rest = data;
        let mut waits = 0;
        while !rest.is_empty() {
            let pool = self.pool.clone();
            let visible = self.visible;
            let mut alloc = move || pool.take(visible);
            match with_decoder!(&mut self.codec, d => d.decode(timestamp, rest, &mut alloc)) {
                Ok(0) => break,
                Ok(n) => rest = rest.get(n..).unwrap_or_default(),
                Err(DecodeError::CheckEvents) => {
                    waits += 1;
                    if waits > MAX_EVENT_WAITS {
                        return Err("the decoder keeps waiting for events".into());
                    }
                    self.drain_events()?;
                }
                Err(DecodeError::NotEnoughOutputBuffers(_)) => {
                    self.drain_events()?;
                    return Err("no free output frame (all are held as references or for display)".into());
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let ended = match &mut self.codec {
            Codec::H264(d) => d.end_access_unit(),
            Codec::H265(d) => d.end_access_unit(),
            Codec::Av1(_) => Ok(()),
        };
        ended.map_err(|e| e.to_string())?;
        self.drain_events()?;
        // Older pictures still in the queue (none in low-delay mode) are
        // superseded by the newest.
        Ok(self.ready.drain(..).last())
    }

    /// Takes the decoder's pending events: pictures into `ready`, format
    /// changes into a new allocation.
    fn drain_events(&mut self) -> Result<(), String> {
        loop {
            let event = with_decoder!(&mut self.codec, d => d.next_event().map(|e| match e {
                DecoderEvent::FrameReady(handle) => Event::Frame(Output {
                    frame: handle.video_frame(),
                }),
                DecoderEvent::FormatChanged => Event::FormatChanged,
            }));
            match event {
                None => return Ok(()),
                Some(Event::Frame(output)) => self.ready.push(output),
                Some(Event::FormatChanged) => self.allocate()?,
            }
        }
    }

    /// (Re)allocates the frames for the stream's current parameters; keeps
    /// the current ones when they still fit.
    fn allocate(&mut self) -> Result<(), String> {
        let info = with_decoder!(&self.codec, d => d.stream_info().cloned())
            .ok_or("no stream information after a format change")?;
        self.visible = info.display_resolution;
        let count = info.min_num_frames + EXTRA_FRAMES;
        let wanted = (info.coded_resolution, count);
        if self.allocation == Some(wanted) {
            return Ok(());
        }
        let coded = info.coded_resolution;
        let surfaces = self
            .display
            .create_surfaces::<()>(
                libva::VA_RT_FORMAT_YUV420,
                Some(libva::VA_FOURCC_NV12),
                coded.width,
                coded.height,
                Some(UsageHint::USAGE_HINT_DECODER),
                vec![(); count],
            )
            .map_err(|e| format!("allocating {count} {}x{} surfaces: {e}", coded.width, coded.height))?;
        self.generation += 1;
        let pool = Arc::new(FramePool::default());
        for (index, surface) in surfaces.iter().enumerate() {
            pool.put(Arc::new(FrameMemory::export(surface, index, self.generation)?));
        }
        info!(
            "VA-API: {count} frames of {}x{} ({}x{} shown)",
            coded.width, coded.height, self.visible.width, self.visible.height
        );
        // Frames of the previous allocation still on screen keep their own
        // memory; they just don't come back to the new pool.
        self.pool = pool;
        self.surfaces = surfaces;
        self.allocation = Some(wanted);
        Ok(())
    }

    /// Drops pending output and references; decoding resumes at the next
    /// keyframe.
    fn flush(&mut self) -> Result<(), String> {
        with_decoder!(&mut self.codec, d => d.flush()).map_err(|e| e.to_string())?;
        self.drain_events()?;
        self.ready.clear();
        Ok(())
    }

    /// Reads a picture back as NV12 and hands its planes to `f`:
    /// `(y, y_stride, uv, uv_stride)`.
    fn read_back<R>(&mut self, frame: &VaFrame, f: impl FnOnce(&[u8], usize, &[u8], usize) -> R) -> Result<R, String> {
        if frame.memory.generation != self.generation {
            return Err("picture from an earlier allocation".into());
        }
        let format = match self.nv12 {
            Some(format) => format,
            None => {
                let format = self
                    .display
                    .query_image_formats()
                    .map_err(|e| format!("listing image formats: {e}"))?
                    .into_iter()
                    .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
                    .ok_or("the driver has no NV12 images")?;
                *self.nv12.insert(format)
            }
        };
        let surface = self
            .surfaces
            .get(frame.memory.index)
            .ok_or("picture without its surface")?;
        let visible = (frame.visible.width, frame.visible.height);
        let image = libva::Image::create_from(surface, format, surface.size(), visible)
            .map_err(|e| format!("reading the picture back: {e}"))?;
        let va = image.image();
        let data: &[u8] = image.as_ref();
        let (y_offset, uv_offset) = (va.offsets[0] as usize, va.offsets[1] as usize);
        let (Some(y), Some(uv)) = (data.get(y_offset..), data.get(uv_offset..)) else {
            return Err("read-back image is shorter than its planes".into());
        };
        Ok(f(y, va.pitches[0] as usize, uv, va.pitches[1] as usize))
    }
}

/// How the last picture went out (for the decoder's name).
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    None,
    ZeroCopy,
    GpuColour,
    CpuColour,
}

/// The VA-API [`FrameDecoder`].
pub struct VaapiDecoder {
    node: PathBuf,
    vendor: String,
    codecs: Vec<VideoCodec>,
    colorimetry: Colorimetry,
    output: Option<Arc<GpuOutput>>,
    planes: Arc<PlanePool>,
    format: Option<VideoFormat>,
    session: Option<Session>,
    mode: OutputMode,
    failed: Option<String>,
}

// SAFETY: the only non-`Send` state is `session` (libva objects counted with
// `Rc`). It is created by the first `decode_timed` call, which `DecodingSink`
// makes on its decoder thread after moving the decoder there, and it is only
// touched on the thread that created it: `session_on_this_thread` refuses
// any other, and `Drop` leaks a session found on another thread instead of
// dropping it there. Everything else is `Send`.
unsafe impl Send for VaapiDecoder {}

impl VaapiDecoder {
    fn new(node: PathBuf, caps: Capabilities, colorimetry: Colorimetry, output: Option<Arc<GpuOutput>>) -> Self {
        Self {
            node,
            vendor: caps.vendor,
            codecs: caps.codecs,
            colorimetry,
            output,
            planes: Arc::new(PlanePool::default()),
            format: None,
            session: None,
            mode: OutputMode::None,
            failed: None,
        }
    }

    /// The formats to offer the host: 8-bit 4:2:0 in every codec the device
    /// decodes.
    pub fn formats(&self) -> Vec<VideoFormat> {
        self.codecs
            .iter()
            .map(|&codec| VideoFormat {
                codec,
                ten_bit: false,
                yuv444: false,
            })
            .collect()
    }

    fn fail(&mut self, why: String) {
        warn!("VA-API decoder stopped: {why}");
        self.failed = Some(why);
    }

    /// The session, created on the first call; `None` (after recording a
    /// failure) if it cannot be opened or belongs to another thread.
    fn session_on_this_thread(&mut self) -> Option<&mut Session> {
        if self.failed.is_some() {
            return None;
        }
        if self.session.is_none() {
            let Some(format) = self.format else {
                self.fail("used before configure".into());
                return None;
            };
            let node = self.node.clone();
            match catch_unwind(|| Session::open(&node, format.codec)) {
                Ok(Ok(session)) => {
                    info!(
                        "VA-API decoding {:?} on {} ({})",
                        format.codec,
                        node.display(),
                        self.vendor
                    );
                    self.session = Some(session);
                }
                Ok(Err(e)) => {
                    self.fail(format!("VA-API: {e}"));
                    return None;
                }
                Err(panic) => {
                    self.fail(format!(
                        "VA-API panicked opening the decoder: {}",
                        panic_message(panic.as_ref())
                    ));
                    return None;
                }
            }
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.thread != std::thread::current().id())
        {
            self.fail("VA-API decoder used from two threads".into());
            return None;
        }
        self.session.as_mut()
    }

    /// Hands `output` on the way the renderer can take it.
    fn present(&mut self, output: Output, out: &mut DecodedFrame) -> Result<(), String> {
        let frame = output.frame;
        let (width, height) = (frame.visible.width, frame.visible.height);
        let colorimetry = self.colorimetry;
        let gpu = self.output.as_ref();
        if gpu.is_some_and(|gpu| gpu.dmabuf()) {
            out.width = width;
            out.height = height;
            out.buffer = FrameBuffer::Native(NativePicture::Dmabuf(frame.dmabuf(colorimetry)));
            self.mode = OutputMode::ZeroCopy;
            return Ok(());
        }
        let session = self.session.as_mut().ok_or("no VA-API session")?;
        if gpu.is_some_and(|gpu| gpu.available()) {
            let pool = &self.planes;
            let picture = session.read_back(&frame, |y, y_stride, uv, uv_stride| {
                PlanarPicture::copy_from_nv12(
                    pool,
                    (y, uv),
                    (y_stride, uv_stride),
                    width as usize,
                    height as usize,
                    colorimetry,
                )
            })??;
            out.width = width;
            out.height = height;
            out.buffer = FrameBuffer::Native(NativePicture::Planar(picture));
            self.mode = OutputMode::GpuColour;
            return Ok(());
        }
        let pixels = out.reshape(width, height);
        session.read_back(&frame, |y, y_stride, uv, uv_stride| {
            let source = Yuv420 {
                y,
                u: uv,
                v: uv.get(1..).unwrap_or_default(),
                y_stride,
                u_stride: uv_stride,
                v_stride: uv_stride,
                uv_pixel_stride: 2,
                width: width as usize,
                height: height as usize,
            };
            yuv420_to_rgba(&source, colorimetry, pixels);
        })?;
        self.mode = OutputMode::CpuColour;
        Ok(())
    }
}

impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && session.thread != std::thread::current().id()
        {
            // Its `Rc`s must not be touched here; leaking it is the safe choice.
            std::mem::forget(session);
        }
    }
}

impl FrameDecoder for VaapiDecoder {
    fn name(&self) -> &'static str {
        match self.mode {
            OutputMode::None => "VA-API (hardware)",
            OutputMode::ZeroCopy => "VA-API (hardware, zero-copy)",
            OutputMode::GpuColour => "VA-API (hardware, copied, GPU colour)",
            OutputMode::CpuColour => "VA-API (hardware, copied, CPU colour)",
        }
    }

    fn supports(&self, format: VideoFormat) -> bool {
        self.codecs.contains(&format.codec) && !format.ten_bit && !format.yuv444
    }

    fn configure(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        if !self.supports(format) {
            return Err(format!("VA-API ({}) cannot decode {format:?}", self.vendor));
        }
        info!("VA-API: {format:?} {width}x{height}@{fps} on {}", self.node.display());
        self.format = Some(format);
        Ok(())
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String> {
        let now = Instant::now();
        self.decode_timed(frame, out, FrameTiming::new(frame, now, now))
            .map(|t| t.is_some())
    }

    fn reset(&mut self) -> Result<(), String> {
        let Some(session) = self.session_on_this_thread() else {
            return Ok(());
        };
        match catch_unwind(AssertUnwindSafe(|| session.flush())) {
            Ok(result) => result,
            Err(panic) => {
                self.fail(format!("VA-API panicked resetting: {}", panic_message(panic.as_ref())));
                Ok(())
            }
        }
    }

    fn terminal_error(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut DecodedFrame,
        mut timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        let Some(session) = self.session_on_this_thread() else {
            return Ok(None);
        };
        timing.codec_input_at = Instant::now();
        let fed = catch_unwind(AssertUnwindSafe(|| {
            session.feed(u64::from(frame.frame_number), &frame.data)
        }));
        let picture = match fed {
            Ok(result) => result?,
            Err(panic) => {
                self.fail(format!("VA-API panicked decoding: {}", panic_message(panic.as_ref())));
                return Ok(None);
            }
        };
        let Some(picture) = picture else {
            return Ok(None);
        };
        timing.codec_output_at = Instant::now();
        match catch_unwind(AssertUnwindSafe(|| self.present(picture, out))) {
            Ok(result) => result?,
            Err(panic) => {
                self.fail(format!(
                    "VA-API panicked reading a picture: {}",
                    panic_message(panic.as_ref())
                ));
                return Ok(None);
            }
        }
        Ok(Some(timing))
    }
}
