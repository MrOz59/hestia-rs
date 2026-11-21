//! DMA-BUF pictures into EGL images, for the desktop GL renderer (Linux).
//!
//! A hardware decoder (VA-API) hands over NV12 pictures in GPU memory as
//! DMA-BUFs ([`DmabufPicture`]). Each becomes two EGL images on the window's
//! EGL display, `R8` for luma and `GR88` for the interleaved chroma (with
//! their format modifiers, and compression planes when the driver has
//! them), which [`YuvPass::draw_nv12`](crate::gl_yuv::YuvPass::draw_nv12)
//! samples like any texture. Nothing is copied.
//!
//! EGL is loaded at run time from `libEGL.so.1` (the library the toolkit's
//! EGL context already uses), so building needs no EGL development files.
//! A window that does not render through EGL (X11 with GLX) has no importer:
//! hardware-decoded pictures are copied instead.

use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::ptr;

use hestia_media::native_picture::{DmabufPicture, DmabufPlane};

type EglDisplay = *mut c_void;
type EglImage = *mut c_void;
type EglInt = i32;
type EglAttrib = isize;

const EGL_NONE: EglInt = 0x3038;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_EXTENSIONS: EglInt = 0x3055;
const EGL_DEVICE_EXT: EglInt = 0x322C;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
const EGL_DRM_RENDER_NODE_FILE_EXT: EglInt = 0x3377;

/// `EGL_DMA_BUF_PLANEn_{FD, OFFSET, PITCH, MODIFIER_LO, MODIFIER_HI}_EXT`
/// for planes 0 and 1 (the main plane and its compression plane).
const PLANE_ATTRIBS: [[EglInt; 5]; 2] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
];

const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

type GetCurrentDisplay = unsafe extern "C" fn() -> EglDisplay;
type QueryString = unsafe extern "C" fn(EglDisplay, EglInt) -> *const c_char;
type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type GetError = unsafe extern "C" fn() -> EglInt;
type CreateImage = unsafe extern "C" fn(EglDisplay, *mut c_void, u32, *mut c_void, *const EglInt) -> EglImage;
type DestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> u32;
type QueryDisplayAttrib = unsafe extern "C" fn(EglDisplay, EglInt, *mut EglAttrib) -> u32;
type QueryDeviceString = unsafe extern "C" fn(*mut c_void, EglInt) -> *const c_char;

/// Imports DMA-BUF pictures on the EGL display current when it was made.
pub struct DmabufImporter {
    display: EglDisplay,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    get_error: GetError,
    /// `EGL_EXT_image_dma_buf_import_modifiers`: tiled and compressed
    /// buffers can be imported.
    modifiers: bool,
    /// The display's GPU, for decoding on the same one.
    pub render_node: Option<PathBuf>,
}

fn has_extension(list: *const c_char, name: &str) -> bool {
    if list.is_null() {
        return false;
    }
    // SAFETY: EGL extension strings are NUL-terminated and static.
    unsafe { CStr::from_ptr(list) }
        .to_string_lossy()
        .split_whitespace()
        .any(|e| e == name)
}

/// Looks `name` up in an already loaded `libEGL.so.1`.
unsafe fn symbol<T: Copy>(lib: *mut c_void, name: &CStr) -> Option<T> {
    // SAFETY: `lib` is a dlopen handle; the caller names the right type.
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

impl DmabufImporter {
    /// Checks the current EGL display; call with the window's GL context
    /// current.
    pub fn new() -> Result<Self, String> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; every function is
        // called with the signature the EGL specification gives it.
        unsafe {
            let lib = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return Err("libEGL.so.1 is not available".into());
            }
            let missing = |name: &str| format!("{name} is unavailable");
            let get_current_display: GetCurrentDisplay =
                symbol(lib, c"eglGetCurrentDisplay").ok_or_else(|| missing("eglGetCurrentDisplay"))?;
            let query_string: QueryString = symbol(lib, c"eglQueryString").ok_or_else(|| missing("eglQueryString"))?;
            let get_proc_address: GetProcAddress =
                symbol(lib, c"eglGetProcAddress").ok_or_else(|| missing("eglGetProcAddress"))?;
            let get_error: GetError = symbol(lib, c"eglGetError").ok_or_else(|| missing("eglGetError"))?;
            let display = get_current_display();
            if display.is_null() {
                return Err("the window does not render through EGL".into());
            }
            let extensions = query_string(display, EGL_EXTENSIONS);
            if !has_extension(extensions, "EGL_EXT_image_dma_buf_import") {
                return Err("EGL_EXT_image_dma_buf_import is missing".into());
            }
            let modifiers = has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
            let proc_address = |name: &CStr| {
                let p = get_proc_address(name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let create_image = proc_address(c"eglCreateImageKHR").ok_or_else(|| missing("eglCreateImageKHR"))?;
            let destroy_image = proc_address(c"eglDestroyImageKHR").ok_or_else(|| missing("eglDestroyImageKHR"))?;
            let render_node = render_node(display, query_string, &proc_address);
            Ok(Self {
                display,
                create_image: std::mem::transmute::<*mut c_void, CreateImage>(create_image),
                destroy_image: std::mem::transmute::<*mut c_void, DestroyImage>(destroy_image),
                get_error,
                modifiers,
                render_node,
            })
        }
    }

    /// The picture's luma and chroma as EGL images, for this frame's draw.
    pub fn import(&self, picture: &DmabufPicture) -> Result<ImportedNv12<'_>, String> {
        let [(y, y_aux), (uv, uv_aux)] = picture
            .luma_chroma()
            .ok_or_else(|| format!("a DMA-BUF picture with {} planes", picture.planes.len()))?;
        let (w, h) = (picture.coded_width, picture.coded_height);
        let luma = self.image(DRM_FORMAT_R8, (w, h), y, y_aux)?;
        let chroma = match self.image(DRM_FORMAT_GR88, (w.div_ceil(2), h.div_ceil(2)), uv, uv_aux) {
            Ok(image) => image,
            Err(e) => {
                // SAFETY: an image this importer created.
                unsafe { (self.destroy_image)(self.display, luma) };
                return Err(e);
            }
        };
        Ok(ImportedNv12 {
            importer: self,
            images: [luma, chroma],
        })
    }

    fn image(
        &self,
        fourcc: u32,
        (width, height): (u32, u32),
        main: DmabufPlane,
        aux: Option<DmabufPlane>,
    ) -> Result<EglImage, String> {
        let mut attribs = vec![
            EGL_WIDTH,
            width as EglInt,
            EGL_HEIGHT,
            height as EglInt,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as EglInt,
        ];
        for (plane, names) in std::iter::once(main).chain(aux).zip(PLANE_ATTRIBS) {
            attribs.extend([
                names[0],
                plane.fd,
                names[1],
                plane.offset as EglInt,
                names[2],
                plane.pitch as EglInt,
            ]);
            if plane.modifier != DRM_FORMAT_MOD_INVALID {
                if self.modifiers {
                    attribs.extend([
                        names[3],
                        plane.modifier as u32 as EglInt,
                        names[4],
                        (plane.modifier >> 32) as u32 as EglInt,
                    ]);
                } else if plane.modifier != DRM_FORMAT_MOD_LINEAR {
                    return Err(format!(
                        "a tiled picture (modifier {:#x}) without EGL_EXT_image_dma_buf_import_modifiers",
                        plane.modifier
                    ));
                }
            }
        }
        attribs.push(EGL_NONE);
        // SAFETY: the attribute list is EGL_NONE-terminated; the fds are
        // valid while the picture lives (EGL duplicates what it keeps).
        let image = unsafe {
            (self.create_image)(
                self.display,
                ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            // SAFETY: a plain query.
            let error = unsafe { (self.get_error)() };
            return Err(format!(
                "eglCreateImageKHR failed ({error:#x}) for a {width}x{height} {:?} plane, modifier {:#x}",
                String::from_utf8_lossy(&fourcc.to_le_bytes()),
                main.modifier
            ));
        }
        Ok(image)
    }
}

/// The display's DRM render node (`EGL_EXT_device_query` and
/// `EGL_EXT_device_drm_render_node`), if EGL tells.
unsafe fn render_node(
    display: EglDisplay,
    query_string: QueryString,
    proc_address: &dyn Fn(&CStr) -> Option<*mut c_void>,
) -> Option<PathBuf> {
    // SAFETY: client extensions are queried on EGL_NO_DISPLAY; the device
    // functions are called with their specified signatures.
    unsafe {
        let client = query_string(ptr::null_mut(), EGL_EXTENSIONS);
        if !has_extension(client, "EGL_EXT_device_query") && !has_extension(client, "EGL_EXT_device_base") {
            return None;
        }
        let query_display: QueryDisplayAttrib =
            std::mem::transmute::<*mut c_void, QueryDisplayAttrib>(proc_address(c"eglQueryDisplayAttribEXT")?);
        let query_device: QueryDeviceString =
            std::mem::transmute::<*mut c_void, QueryDeviceString>(proc_address(c"eglQueryDeviceStringEXT")?);
        let mut device: EglAttrib = 0;
        if query_display(display, EGL_DEVICE_EXT, &mut device) == 0 || device == 0 {
            return None;
        }
        let device = device as *mut c_void;
        if !has_extension(query_device(device, EGL_EXTENSIONS), "EGL_EXT_device_drm_render_node") {
            return None;
        }
        let node = query_device(device, EGL_DRM_RENDER_NODE_FILE_EXT);
        (!node.is_null()).then(|| PathBuf::from(CStr::from_ptr(node).to_string_lossy().into_owned()))
    }
}

/// A picture's two EGL images; destroyed when dropped (textures they were
/// bound to keep the memory until they are respecified).
pub struct ImportedNv12<'a> {
    importer: &'a DmabufImporter,
    /// Luma (`R8`), chroma (`GR88`).
    pub images: [*mut c_void; 2],
}

impl Drop for ImportedNv12<'_> {
    fn drop(&mut self) {
        for image in self.images {
            // SAFETY: images this importer created, destroyed once.
            unsafe { (self.importer.destroy_image)(self.importer.display, image) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_fourccs_match_the_kernel_codes() {
        assert_eq!(DRM_FORMAT_R8, 0x2020_3852);
        assert_eq!(DRM_FORMAT_GR88, 0x3838_5247);
    }
}
