//! Narrow bindings to the Hermes-pinned PyroWave 0.5 C API. The runtime is
//! loaded only for an explicit PyroWave stream. All calls for one device are
//! serialized, first by session setup and then by the decode worker.
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use ash::vk;
use libloading::Library;

use crate::color::{Colorimetry, I420, i420_to_rgba};
use crate::decode::FrameTiming;
use crate::native_picture::{GpuOutput, NativePicture, PlanarPicture, PlanePool};
use crate::{DecodedFrame, DecoderChoice, EncodedFrame, FrameBuffer, FrameDecoder, VideoCodec, VideoFormat};

type Handle = *mut c_void;
type Destroy = unsafe extern "C" fn(Handle);
const FORMAT: VideoFormat = VideoFormat {
    codec: VideoCodec::PyroWave,
    ten_bit: false,
    yuv444: false,
};

#[repr(C)]
struct DecoderInfo {
    device: Handle,
    width: c_int,
    height: c_int,
    chroma: c_int,
    fragment_path: bool,
}

#[repr(C)]
struct CpuBuffer {
    data: [*mut u8; 3],
    strides: [usize; 3],
    sizes: [usize; 3],
    width: c_int,
    height: c_int,
    format: c_int,
}

struct Api {
    device_create: unsafe extern "C" fn(*mut Handle) -> c_int,
    device_destroy: Destroy,
    vk_handles: unsafe extern "C" fn(Handle, *mut vk::Instance, *mut vk::PhysicalDevice, *mut vk::Device),
    prefers_fragment: unsafe extern "C" fn(Handle) -> bool,
    decoder_create: unsafe extern "C" fn(*const DecoderInfo, *mut Handle) -> c_int,
    decoder_destroy: Destroy,
    clear: unsafe extern "C" fn(Handle),
    push: unsafe extern "C" fn(Handle, *const u8, usize) -> c_int,
    ready: unsafe extern "C" fn(Handle, bool) -> bool,
    decode: unsafe extern "C" fn(Handle, *const CpuBuffer) -> c_int,
    _library: Library,
}

impl Api {
    fn load() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let library = {
            let name = "libpyrowave-shared.so.0";
            let bundled = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .with_file_name("lib")
                .join(name);
            // SAFETY: load the packaged SDK, or a system installation by SONAME.
            if bundled.is_file() {
                unsafe { Library::new(&bundled) }.map_err(|e| format!("Cannot load {}: {e}", bundled.display()))
            } else {
                unsafe { Library::new(name) }.map_err(|e| {
                    format!(
                        "PyroWave needs {name}. Use the Linux AppImage or extract the whole tarball with lib/ beside the executable. Expected {}. System loader: {e}",
                        bundled.display()
                    )
                })
            }
        }?;
        #[cfg(target_os = "android")]
        let library = unsafe { Library::new("libpyrowave-shared.so") }
            .map_err(|e| format!("Cannot load the PyroWave runtime: {e}"))?;
        // SAFETY: get_api_version is the stable entry point. All other types
        // below mirror pyrowave.h at the checked 0.5 ABI. Keep the library
        // alive until after both native handles are destroyed.
        unsafe {
            let version = library
                .get::<unsafe extern "C" fn(*mut u32, *mut u32, *mut u32)>(b"pyrowave_get_api_version\0")
                .map_err(|e| e.to_string())?;
            let (mut major, mut minor, mut patch) = (0, 0, 0);
            version(&mut major, &mut minor, &mut patch);
            if (major, minor) != (0, 5) {
                return Err(format!(
                    "PyroWave runtime {major}.{minor}.{patch} is incompatible (need 0.5.x)"
                ));
            }
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!("pyrowave_", $name, "\0").as_bytes())
                        .map_err(|e| e.to_string())?
                };
            }
            Ok(Self {
                device_create: symbol!("create_default_device"),
                device_destroy: symbol!("device_destroy"),
                vk_handles: symbol!("device_get_vk_device_handles"),
                prefers_fragment: symbol!("decoder_device_prefers_fragment_path"),
                decoder_create: symbol!("decoder_create"),
                decoder_destroy: symbol!("decoder_destroy"),
                clear: symbol!("decoder_clear"),
                push: symbol!("decoder_push_packet"),
                ready: symbol!("decoder_decode_is_ready"),
                decode: symbol!("decoder_decode_cpu_buffer_synchronous"),
                _library: library,
            })
        }
    }
}

struct Decoder {
    api: Api,
    device: Handle,
    decoder: Handle,
    width: u32,
    height: u32,
    gpu: Option<Arc<GpuOutput>>,
    pool: Arc<PlanePool>,
}

// SAFETY: native handles have exclusive ownership and no thread affinity.
// The synchronous API finishes all work using the caller's buffers before returning.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.decoder.is_null() {
                (self.api.decoder_destroy)(self.decoder);
            }
            if !self.device.is_null() {
                (self.api.device_destroy)(self.device);
            }
        }
    }
}

fn check(status: c_int, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("PyroWave {operation} failed ({status})"))
    }
}

pub(crate) fn open(width: u32, height: u32, gpu: Option<Arc<GpuOutput>>) -> Result<DecoderChoice, String> {
    open_device(width, height, gpu, false)
}

fn open_device(
    width: u32,
    height: u32,
    gpu: Option<Arc<GpuOutput>>,
    allow_software_icd: bool,
) -> Result<DecoderChoice, String> {
    if !crate::video_decoder::PYROWAVE_SUPPORTED {
        return Err("PyroWave needs a 64-bit Android app and Vulkan GPU".into());
    }
    if ![width, height]
        .into_iter()
        .all(|d| (2..=8192).contains(&d) && d % 2 == 0)
    {
        return Err("PyroWave needs even dimensions between 2 and 8192".into());
    }
    let mut session = Decoder {
        api: Api::load()?,
        device: ptr::null_mut(),
        decoder: ptr::null_mut(),
        width,
        height,
        gpu,
        pool: Arc::default(),
    };
    // SAFETY: the create calls initialize our handles on success. Only the
    // decoder borrows the Vulkan device, and Drop destroys it first.
    unsafe {
        check(
            (session.api.device_create)(&mut session.device),
            "Vulkan device creation",
        )?;
        // Software ICDs can support device creation yet fail during the SDK's
        // shader dispatch. Interactive streams require hardware decoding.
        let mut instance = vk::Instance::null();
        let mut physical = vk::PhysicalDevice::null();
        (session.api.vk_handles)(session.device, &mut instance, &mut physical, ptr::null_mut());
        let entry = ash::Entry::load().map_err(|e| format!("Cannot query the Vulkan device: {e}"))?;
        let instance = ash::Instance::load(entry.static_fn(), instance);
        let props = instance.get_physical_device_properties(physical);
        if props.device_type == vk::PhysicalDeviceType::CPU && !allow_software_icd {
            return Err("PyroWave requires a hardware Vulkan GPU. The Vulkan driver selected a CPU renderer.".into());
        }
        let info = DecoderInfo {
            device: session.device,
            width: width as c_int,
            height: height as c_int,
            chroma: 0, // PYROWAVE_CHROMA_SUBSAMPLING_420
            fragment_path: (session.api.prefers_fragment)(session.device),
        };
        check(
            (session.api.decoder_create)(&info, &mut session.decoder),
            "Vulkan decoder creation",
        )?;
    }
    Ok((Box::new(session), vec![FORMAT]))
}

impl FrameDecoder for Decoder {
    fn name(&self) -> &'static str {
        "PyroWave (Vulkan, I420 readback)"
    }
    fn supports(&self, format: VideoFormat) -> bool {
        format == FORMAT
    }
    fn configure(&mut self, format: VideoFormat, width: u32, height: u32, _: u32) -> Result<(), String> {
        if !self.supports(format) || (width, height) != (self.width, self.height) {
            return Err("PyroWave decoder and negotiated video mode differ".into());
        }
        Ok(())
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String> {
        let now = Instant::now();
        self.decode_timed(frame, out, FrameTiming::new(frame, now, now))
            .map(|result| result.is_some())
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut DecodedFrame,
        mut timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        if frame.data.is_empty() || frame.data.len() > 64 * 1024 * 1024 {
            return Err("Invalid PyroWave frame length".into());
        }
        // SAFETY: complete owned access unit; the SDK copies the packet.
        unsafe {
            (self.api.clear)(self.decoder);
            check(
                (self.api.push)(self.decoder, frame.data.as_ptr(), frame.data.len()),
                "packet submission",
            )?;
            if !(self.api.ready)(self.decoder, false) {
                return Err("Incomplete PyroWave frame".into());
            }
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let color = Colorimetry::Bt709Limited;
        let mut picture = PlanarPicture::allocate(&self.pool, self.width, self.height, color);
        let (y, chroma) = picture.bytes_mut().split_at_mut(w * h);
        let (u, v) = chroma.split_at_mut(w * h / 4);
        let target = CpuBuffer {
            data: [y.as_mut_ptr(), u.as_mut_ptr(), v.as_mut_ptr()],
            sizes: [y.len(), u.len(), v.len()],
            strides: [w, w / 2, w / 2],
            width: w as c_int,
            height: h as c_int,
            format: 1, // PYROWAVE_CPU_BUFFER_FORMAT_YUV420P
        };
        // SAFETY: non-overlapping, tightly packed I420 planes of exactly the
        // configured size. The SDK completes the writes before returning.
        check(unsafe { (self.api.decode)(self.decoder, &target) }, "Vulkan decode")?;
        timing.codec_output_at = Instant::now();
        if self.gpu.as_ref().is_some_and(|gpu| gpu.available()) {
            out.width = self.width;
            out.height = self.height;
            out.buffer = FrameBuffer::Native(NativePicture::Planar(picture));
        } else {
            i420_to_rgba(
                &I420 {
                    y,
                    u,
                    v,
                    y_stride: w,
                    uv_stride: w / 2,
                    width: w,
                    height: h,
                },
                color,
                out.reshape(self.width, self.height),
            );
        }
        Ok(Some(timing))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_picture::GpuRendererStatus;

    #[test]
    fn rejects_invalid_dimensions_before_opening_a_device() {
        for size in [(0, 720), (1279, 720), (1280, 721), (8194, 2)] {
            assert!(open(size.0, size.1, None).is_err());
        }
    }

    // Packet produced by the pinned Hermes encoder API, neutral chroma and
    // two vertical stripes of limited-range luma (40 and 200).
    #[test]
    #[ignore = "requires the PyroWave 0.5 runtime and a Vulkan ICD"]
    fn decodes_reference_frame_on_vulkan() {
        let status = Arc::new(GpuRendererStatus::default());
        let (mut decoder, formats) =
            open_device(128, 96, Some(Arc::new(GpuOutput::new(status.clone()))), true).unwrap();
        assert_eq!(formats, [FORMAT]);
        // Setup happens on the launch thread. The decode worker owns the
        // handles for all frame submissions and destroys them on exit.
        std::thread::spawn(move || {
            let mut frame = EncodedFrame {
                data: include_bytes!("../tests/data/luma-bars-128x96.pw").to_vec(),
                frame_number: 1,
                keyframe: true,
                received_at: Instant::now(),
                idr_requested_at: None,
                loss_detected_at: None,
                host_latency: None,
            };
            let mut out = DecodedFrame::default();
            status.ready();
            assert!(decoder.decode(&frame, &mut out).unwrap());
            let FrameBuffer::Native(NativePicture::Planar(picture)) = &out.buffer else {
                panic!("expected planar output")
            };
            let (y, u, v) = picture.planes();
            for (i, &luma) in y.iter().enumerate() {
                assert!(luma.abs_diff(if i % 128 < 64 { 40 } else { 200 }) <= 2);
            }
            assert!(u.iter().chain(v).all(|n| n.abs_diff(128) <= 2));
            let packet = std::mem::take(&mut frame.data);
            assert!(decoder.decode(&frame, &mut out).is_err());
            frame.frame_number += 2;
            frame.data = packet;
            status.pause();
            assert!(decoder.decode(&frame, &mut out).unwrap());
            assert_eq!((out.width, out.height), (128, 96));
            let rgba = out.rgba().unwrap();
            assert!(rgba[0].abs_diff(28) <= 3 && rgba[64 * 4].abs_diff(214) <= 3);
        })
        .join()
        .unwrap();
    }
}
