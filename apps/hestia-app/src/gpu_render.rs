//! Zero-copy video on Android: draws hardware-decoded pictures into the
//! texture the stream view shows.
//!
//! MediaCodec renders into `AHardwareBuffer`s (see `hestia_media::gpu_video`).
//! Inside Slint's rendering notifier, with its GL context current, each new
//! picture becomes an `EGLImage` bound to an external (`samplerExternalOES`)
//! texture; one shader pass draws it into an RGBA texture, the GPU sampler
//! doing the YUV to RGB conversion in the colour space the decoder tagged.
//! Slint shows that texture as the stream picture. Every bit of GL state the
//! pass touches is put back, since Skia caches it.
//!
//! Anything failing here (missing extensions, a GL error) marks the GPU path
//! failed: decoders switch back to CPU output and the stream goes on.

use std::ffi::{CStr, c_char, c_void};
use std::num::NonZeroU32;
use std::ptr;

use hestia_media::gpu_video::{GpuPicture, GpuVideo};
use log::{info, warn};
use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;

#[allow(non_snake_case, dead_code, clippy::upper_case_acronyms)]
mod ffi {
    use std::ffi::{c_char, c_int, c_uchar, c_uint, c_void};

    pub type GLenum = c_uint;
    pub type GLuint = c_uint;
    pub type GLint = c_int;
    pub type GLsizei = c_int;
    pub type GLboolean = c_uchar;
    pub type GLfloat = f32;
    pub type GLsizeiptr = isize;

    pub const GL_NO_ERROR: GLenum = 0;
    pub const GL_FALSE: GLboolean = 0;
    pub const GL_TRUE: GLboolean = 1;
    pub const GL_TRIANGLE_STRIP: GLenum = 0x0005;
    pub const GL_CULL_FACE: GLenum = 0x0B44;
    pub const GL_DEPTH_TEST: GLenum = 0x0B71;
    pub const GL_STENCIL_TEST: GLenum = 0x0B90;
    pub const GL_VIEWPORT: GLenum = 0x0BA2;
    pub const GL_BLEND: GLenum = 0x0BE2;
    pub const GL_SCISSOR_TEST: GLenum = 0x0C11;
    pub const GL_COLOR_WRITEMASK: GLenum = 0x0C23;
    pub const GL_TEXTURE_2D: GLenum = 0x0DE1;
    pub const GL_UNSIGNED_BYTE: GLenum = 0x1401;
    pub const GL_FLOAT: GLenum = 0x1406;
    pub const GL_RGBA: GLenum = 0x1908;
    pub const GL_VERSION: GLenum = 0x1F02;
    pub const GL_EXTENSIONS: GLenum = 0x1F03;
    pub const GL_LINEAR: GLint = 0x2601;
    pub const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
    pub const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
    pub const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
    pub const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
    pub const GL_RGBA8: GLint = 0x8058;
    pub const GL_TEXTURE_BINDING_2D: GLenum = 0x8069;
    pub const GL_CLAMP_TO_EDGE: GLint = 0x812F;
    pub const GL_TEXTURE0: GLenum = 0x84C0;
    pub const GL_ACTIVE_TEXTURE: GLenum = 0x84E0;
    pub const GL_VERTEX_ARRAY_BINDING: GLenum = 0x85B5;
    pub const GL_ARRAY_BUFFER: GLenum = 0x8892;
    pub const GL_ARRAY_BUFFER_BINDING: GLenum = 0x8894;
    pub const GL_STATIC_DRAW: GLenum = 0x88E4;
    pub const GL_PIXEL_UNPACK_BUFFER: GLenum = 0x88EC;
    pub const GL_PIXEL_UNPACK_BUFFER_BINDING: GLenum = 0x88EF;
    pub const GL_SAMPLER_BINDING: GLenum = 0x8919;
    pub const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
    pub const GL_VERTEX_SHADER: GLenum = 0x8B31;
    pub const GL_COMPILE_STATUS: GLenum = 0x8B81;
    pub const GL_LINK_STATUS: GLenum = 0x8B82;
    pub const GL_CURRENT_PROGRAM: GLenum = 0x8B8D;
    pub const GL_DRAW_FRAMEBUFFER_BINDING: GLenum = 0x8CA6;
    pub const GL_READ_FRAMEBUFFER: GLenum = 0x8CA8;
    pub const GL_DRAW_FRAMEBUFFER: GLenum = 0x8CA9;
    pub const GL_READ_FRAMEBUFFER_BINDING: GLenum = 0x8CAA;
    pub const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
    pub const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
    pub const GL_FRAMEBUFFER: GLenum = 0x8D40;
    pub const GL_TEXTURE_EXTERNAL_OES: GLenum = 0x8D65;
    pub const GL_TEXTURE_BINDING_EXTERNAL_OES: GLenum = 0x8D67;

    pub const EGL_NONE: c_int = 0x3038;
    pub const EGL_EXTENSIONS: c_int = 0x3055;
    pub const EGL_IMAGE_PRESERVED_KHR: c_int = 0x30D2;
    pub const EGL_NATIVE_BUFFER_ANDROID: c_uint = 0x3140;
    pub const EGL_TRUE: c_int = 1;

    #[link(name = "GLESv3")]
    unsafe extern "C" {
        pub fn glGetError() -> GLenum;
        pub fn glGetString(name: GLenum) -> *const c_uchar;
        pub fn glGetIntegerv(pname: GLenum, data: *mut GLint);
        pub fn glGetBooleanv(pname: GLenum, data: *mut GLboolean);
        pub fn glIsEnabled(cap: GLenum) -> GLboolean;
        pub fn glEnable(cap: GLenum);
        pub fn glDisable(cap: GLenum);
        pub fn glColorMask(r: GLboolean, g: GLboolean, b: GLboolean, a: GLboolean);
        pub fn glViewport(x: GLint, y: GLint, width: GLsizei, height: GLsizei);
        pub fn glActiveTexture(texture: GLenum);
        pub fn glGenTextures(n: GLsizei, textures: *mut GLuint);
        pub fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
        pub fn glBindTexture(target: GLenum, texture: GLuint);
        pub fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
        pub fn glTexImage2D(
            target: GLenum,
            level: GLint,
            internalformat: GLint,
            width: GLsizei,
            height: GLsizei,
            border: GLint,
            format: GLenum,
            type_: GLenum,
            pixels: *const c_void,
        );
        pub fn glBindSampler(unit: GLuint, sampler: GLuint);
        pub fn glGenFramebuffers(n: GLsizei, framebuffers: *mut GLuint);
        pub fn glDeleteFramebuffers(n: GLsizei, framebuffers: *const GLuint);
        pub fn glBindFramebuffer(target: GLenum, framebuffer: GLuint);
        pub fn glFramebufferTexture2D(
            target: GLenum,
            attachment: GLenum,
            textarget: GLenum,
            texture: GLuint,
            level: GLint,
        );
        pub fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
        pub fn glCreateShader(type_: GLenum) -> GLuint;
        pub fn glShaderSource(shader: GLuint, count: GLsizei, string: *const *const c_char, length: *const GLint);
        pub fn glCompileShader(shader: GLuint);
        pub fn glGetShaderiv(shader: GLuint, pname: GLenum, params: *mut GLint);
        pub fn glGetShaderInfoLog(shader: GLuint, size: GLsizei, length: *mut GLsizei, log: *mut c_char);
        pub fn glDeleteShader(shader: GLuint);
        pub fn glCreateProgram() -> GLuint;
        pub fn glAttachShader(program: GLuint, shader: GLuint);
        pub fn glBindAttribLocation(program: GLuint, index: GLuint, name: *const c_char);
        pub fn glLinkProgram(program: GLuint);
        pub fn glGetProgramiv(program: GLuint, pname: GLenum, params: *mut GLint);
        pub fn glGetProgramInfoLog(program: GLuint, size: GLsizei, length: *mut GLsizei, log: *mut c_char);
        pub fn glDeleteProgram(program: GLuint);
        pub fn glUseProgram(program: GLuint);
        pub fn glGetUniformLocation(program: GLuint, name: *const c_char) -> GLint;
        pub fn glUniform1i(location: GLint, v0: GLint);
        pub fn glUniform4f(location: GLint, v0: GLfloat, v1: GLfloat, v2: GLfloat, v3: GLfloat);
        pub fn glGenBuffers(n: GLsizei, buffers: *mut GLuint);
        pub fn glDeleteBuffers(n: GLsizei, buffers: *const GLuint);
        pub fn glBindBuffer(target: GLenum, buffer: GLuint);
        pub fn glBufferData(target: GLenum, size: GLsizeiptr, data: *const c_void, usage: GLenum);
        pub fn glGenVertexArrays(n: GLsizei, arrays: *mut GLuint);
        pub fn glDeleteVertexArrays(n: GLsizei, arrays: *const GLuint);
        pub fn glBindVertexArray(array: GLuint);
        pub fn glVertexAttribPointer(
            index: GLuint,
            size: GLint,
            type_: GLenum,
            normalized: GLboolean,
            stride: GLsizei,
            pointer: *const c_void,
        );
        pub fn glEnableVertexAttribArray(index: GLuint);
        pub fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
    }

    #[link(name = "EGL")]
    unsafe extern "C" {
        pub fn eglGetCurrentDisplay() -> *mut c_void;
        pub fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
        pub fn eglQueryString(display: *mut c_void, name: c_int) -> *const c_char;
    }

    /// `eglGetNativeClientBufferANDROID` (EGL_ANDROID_get_native_client_buffer).
    pub type GetNativeClientBuffer = unsafe extern "C" fn(buffer: *const c_void) -> *mut c_void;
    /// `eglCreateImageKHR` (EGL_KHR_image_base).
    pub type CreateImage = unsafe extern "C" fn(
        display: *mut c_void,
        context: *mut c_void,
        target: c_uint,
        buffer: *mut c_void,
        attribs: *const c_int,
    ) -> *mut c_void;
    /// `eglDestroyImageKHR`.
    pub type DestroyImage = unsafe extern "C" fn(display: *mut c_void, image: *mut c_void) -> c_uint;
    /// `glEGLImageTargetTexture2DOES` (GL_OES_EGL_image_external).
    pub type ImageTargetTexture = unsafe extern "C" fn(target: GLenum, image: *mut c_void);
}

use ffi::*;

const VERTEX_SHADER: &CStr = c"attribute vec2 a_pos;
uniform vec4 u_crop;
varying vec2 v_uv;
void main() {
    v_uv = u_crop.xy + (a_pos * 0.5 + 0.5) * u_crop.zw;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}";

// Row 0 of the hardware buffer (the picture's top) lands in row 0 of the
// target texture, the top-left origin Slint expects.
const FRAGMENT_SHADER: &CStr = c"#extension GL_OES_EGL_image_external : require
precision mediump float;
uniform samplerExternalOES u_tex;
varying vec2 v_uv;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv);
}";

/// Extension entry points, looked up once per GL context.
struct Extensions {
    get_native_client_buffer: GetNativeClientBuffer,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture: ImageTargetTexture,
}

/// GL objects of the conversion pass (created on first use, inside the
/// saved-state section).
#[derive(Default)]
struct Objects {
    program: GLuint,
    u_tex: GLint,
    u_crop: GLint,
    vbo: GLuint,
    vao: GLuint,
    external: GLuint,
    fbo: GLuint,
    target: GLuint,
    size: (u32, u32),
}

struct Gl {
    ext: Extensions,
    objects: Option<Objects>,
}

fn gl_string(name: GLenum) -> String {
    // SAFETY: a context is current (rendering notifier); the string is static.
    let s = unsafe { glGetString(name) };
    if s.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(s.cast()) }.to_string_lossy().into_owned()
}

/// Loads an extension function by name; None when the driver lacks it.
fn proc_address<T: Copy>(name: &CStr) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    // SAFETY: eglGetProcAddress takes a NUL-terminated name.
    let p = unsafe { eglGetProcAddress(name.as_ptr()) };
    // SAFETY: T is the matching function pointer type for `name`.
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

impl Gl {
    /// Checks the current context can draw hardware buffers.
    fn new() -> Result<Self, String> {
        let version = gl_string(GL_VERSION);
        // GLES 3 for vertex array objects and sampler bindings.
        let es3 = version
            .split_whitespace()
            .find_map(|w| w.split('.').next()?.parse::<u32>().ok())
            .is_some_and(|major| major >= 3);
        if !version.starts_with("OpenGL ES") || !es3 {
            return Err(format!("needs OpenGL ES 3, the context is {version:?}"));
        }
        let gl_ext = gl_string(GL_EXTENSIONS);
        if !gl_ext.split_whitespace().any(|e| e == "GL_OES_EGL_image_external") {
            return Err("GL_OES_EGL_image_external is missing".into());
        }
        // SAFETY: plain EGL queries on the current display.
        let display = unsafe { eglGetCurrentDisplay() };
        let egl_ext = unsafe { eglQueryString(display, EGL_EXTENSIONS) };
        let egl_ext = if egl_ext.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(egl_ext) }.to_string_lossy().into_owned()
        };
        for needed in [
            "EGL_KHR_image_base",
            "EGL_ANDROID_image_native_buffer",
            "EGL_ANDROID_get_native_client_buffer",
        ] {
            if !egl_ext.split_whitespace().any(|e| e == needed) {
                return Err(format!("{needed} is missing"));
            }
        }
        let missing = |name: &str| format!("{name} is unavailable");
        let ext = Extensions {
            get_native_client_buffer: proc_address(c"eglGetNativeClientBufferANDROID")
                .ok_or_else(|| missing("eglGetNativeClientBufferANDROID"))?,
            create_image: proc_address(c"eglCreateImageKHR").ok_or_else(|| missing("eglCreateImageKHR"))?,
            destroy_image: proc_address(c"eglDestroyImageKHR").ok_or_else(|| missing("eglDestroyImageKHR"))?,
            image_target_texture: proc_address(c"glEGLImageTargetTexture2DOES")
                .ok_or_else(|| missing("glEGLImageTargetTexture2DOES"))?,
        };
        info!("zero-copy video ready ({version})");
        Ok(Self { ext, objects: None })
    }

    /// Draws `picture` into the target texture. Returns the texture and
    /// whether it was (re)allocated (the stream view needs a new image).
    fn draw(&mut self, picture: &GpuPicture) -> Result<(GLuint, bool), String> {
        let buffer = picture.hardware_buffer();
        if buffer.is_null() {
            return Err("the picture has no hardware buffer".into());
        }
        // SAFETY: the buffer lives as long as `picture`; the context is current.
        unsafe {
            let client = (self.ext.get_native_client_buffer)(buffer);
            if client.is_null() {
                return Err("eglGetNativeClientBufferANDROID failed".into());
            }
            let display = eglGetCurrentDisplay();
            let attribs = [EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE];
            let image = (self.ext.create_image)(
                display,
                ptr::null_mut(),
                EGL_NATIVE_BUFFER_ANDROID,
                client,
                attribs.as_ptr(),
            );
            if image.is_null() {
                return Err("eglCreateImageKHR failed for the decoder's buffer".into());
            }
            // Earlier errors are Skia's business, not this pass's.
            for _ in 0..16 {
                if glGetError() == GL_NO_ERROR {
                    break;
                }
            }
            let saved = SavedState::save();
            let result = self.draw_image(image, picture);
            saved.restore();
            // The texture keeps the buffer referenced for the draw.
            (self.ext.destroy_image)(display, image);
            result
        }
    }

    /// The pass itself; GL state is saved by the caller.
    unsafe fn draw_image(&mut self, image: *mut c_void, picture: &GpuPicture) -> Result<(GLuint, bool), String> {
        unsafe {
            if self.objects.is_none() {
                self.objects = Some(create_objects()?);
            }
            let Some(o) = self.objects.as_mut() else {
                return Err("no GL objects".into());
            };
            glActiveTexture(GL_TEXTURE0);
            glBindSampler(0, 0);
            let size = (picture.width, picture.height);
            let resized = o.size != size;
            if resized {
                glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
                glBindTexture(GL_TEXTURE_2D, o.target);
                glTexImage2D(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA8,
                    size.0 as GLsizei,
                    size.1 as GLsizei,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    ptr::null(),
                );
                glBindFramebuffer(GL_FRAMEBUFFER, o.fbo);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, o.target, 0);
                let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
                if status != GL_FRAMEBUFFER_COMPLETE {
                    return Err(format!("video framebuffer incomplete ({status:#x})"));
                }
                o.size = size;
            }
            glBindFramebuffer(GL_FRAMEBUFFER, o.fbo);
            glBindTexture(GL_TEXTURE_EXTERNAL_OES, o.external);
            (self.ext.image_target_texture)(GL_TEXTURE_EXTERNAL_OES, image);
            glViewport(0, 0, size.0 as GLsizei, size.1 as GLsizei);
            for cap in [GL_BLEND, GL_SCISSOR_TEST, GL_DEPTH_TEST, GL_STENCIL_TEST, GL_CULL_FACE] {
                glDisable(cap);
            }
            glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
            glUseProgram(o.program);
            glUniform1i(o.u_tex, 0);
            let [x, y, w, h] = picture.crop;
            glUniform4f(o.u_crop, x, y, w, h);
            glBindVertexArray(o.vao);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            match glGetError() {
                GL_NO_ERROR => Ok((o.target, resized)),
                e => Err(format!("GL error {e:#x} drawing the video")),
            }
        }
    }

    /// Frees the GL objects (the context is current, being torn down).
    fn delete(&mut self) {
        let Some(o) = self.objects.take() else { return };
        // SAFETY: names created in this context; deleting unbound objects.
        unsafe {
            glDeleteProgram(o.program);
            glDeleteBuffers(1, &o.vbo);
            glDeleteVertexArrays(1, &o.vao);
            glDeleteTextures(1, &o.external);
            glDeleteTextures(1, &o.target);
            glDeleteFramebuffers(1, &o.fbo);
        }
    }
}

unsafe fn compile(kind: GLenum, source: &CStr) -> Result<GLuint, String> {
    unsafe {
        let shader = glCreateShader(kind);
        let src = source.as_ptr();
        glShaderSource(shader, 1, &src, ptr::null());
        glCompileShader(shader);
        let mut ok = 0;
        glGetShaderiv(shader, GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = [0 as c_char; 512];
            glGetShaderInfoLog(shader, log.len() as GLsizei, ptr::null_mut(), log.as_mut_ptr());
            glDeleteShader(shader);
            return Err(format!(
                "video shader: {}",
                CStr::from_ptr(log.as_ptr()).to_string_lossy()
            ));
        }
        Ok(shader)
    }
}

/// Program, quad and textures; bindings it changes are restored by the
/// caller's saved state.
unsafe fn create_objects() -> Result<Objects, String> {
    unsafe {
        let vs = compile(GL_VERTEX_SHADER, VERTEX_SHADER)?;
        let fs = match compile(GL_FRAGMENT_SHADER, FRAGMENT_SHADER) {
            Ok(fs) => fs,
            Err(e) => {
                glDeleteShader(vs);
                return Err(e);
            }
        };
        let program = glCreateProgram();
        glAttachShader(program, vs);
        glAttachShader(program, fs);
        glBindAttribLocation(program, 0, c"a_pos".as_ptr());
        glLinkProgram(program);
        glDeleteShader(vs);
        glDeleteShader(fs);
        let mut ok = 0;
        glGetProgramiv(program, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut log = [0 as c_char; 512];
            glGetProgramInfoLog(program, log.len() as GLsizei, ptr::null_mut(), log.as_mut_ptr());
            glDeleteProgram(program);
            return Err(format!(
                "video program: {}",
                CStr::from_ptr(log.as_ptr()).to_string_lossy()
            ));
        }
        let mut o = Objects {
            program,
            u_tex: glGetUniformLocation(program, c"u_tex".as_ptr()),
            u_crop: glGetUniformLocation(program, c"u_crop".as_ptr()),
            ..Objects::default()
        };
        // A full-screen quad, in its own vertex array.
        glGenVertexArrays(1, &mut o.vao);
        glBindVertexArray(o.vao);
        glGenBuffers(1, &mut o.vbo);
        glBindBuffer(GL_ARRAY_BUFFER, o.vbo);
        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        glBufferData(
            GL_ARRAY_BUFFER,
            std::mem::size_of_val(&quad) as GLsizeiptr,
            quad.as_ptr().cast(),
            GL_STATIC_DRAW,
        );
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, ptr::null());
        glEnableVertexAttribArray(0);

        glActiveTexture(GL_TEXTURE0);
        glGenTextures(1, &mut o.external);
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, o.external);
        for (name, value) in [
            (GL_TEXTURE_MIN_FILTER, GL_LINEAR),
            (GL_TEXTURE_MAG_FILTER, GL_LINEAR),
            (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE),
            (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE),
        ] {
            glTexParameteri(GL_TEXTURE_EXTERNAL_OES, name, value);
        }
        glGenTextures(1, &mut o.target);
        glBindTexture(GL_TEXTURE_2D, o.target);
        for (name, value) in [
            (GL_TEXTURE_MIN_FILTER, GL_LINEAR),
            (GL_TEXTURE_MAG_FILTER, GL_LINEAR),
            (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE),
            (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE),
        ] {
            glTexParameteri(GL_TEXTURE_2D, name, value);
        }
        glGenFramebuffers(1, &mut o.fbo);
        match glGetError() {
            GL_NO_ERROR => Ok(o),
            e => Err(format!("GL error {e:#x} setting up the video pass")),
        }
    }
}

/// The GL state the pass changes, as Skia left it.
struct SavedState {
    draw_framebuffer: GLint,
    read_framebuffer: GLint,
    viewport: [GLint; 4],
    program: GLint,
    active_texture: GLint,
    texture_2d: GLint,
    texture_external: GLint,
    sampler: GLint,
    array_buffer: GLint,
    unpack_buffer: GLint,
    vertex_array: GLint,
    enabled: [(GLenum, GLboolean); 5],
    color_mask: [GLboolean; 4],
}

impl SavedState {
    unsafe fn save() -> Self {
        unsafe {
            let get = |name| {
                let mut v = 0;
                glGetIntegerv(name, &mut v);
                v
            };
            let active_texture = get(GL_ACTIVE_TEXTURE);
            // Texture and sampler bindings are per unit; the pass uses unit 0.
            glActiveTexture(GL_TEXTURE0);
            let mut viewport = [0; 4];
            glGetIntegerv(GL_VIEWPORT, viewport.as_mut_ptr());
            let mut color_mask = [GL_TRUE; 4];
            glGetBooleanv(GL_COLOR_WRITEMASK, color_mask.as_mut_ptr());
            let enabled = [GL_BLEND, GL_SCISSOR_TEST, GL_DEPTH_TEST, GL_STENCIL_TEST, GL_CULL_FACE]
                .map(|cap| (cap, glIsEnabled(cap)));
            Self {
                draw_framebuffer: get(GL_DRAW_FRAMEBUFFER_BINDING),
                read_framebuffer: get(GL_READ_FRAMEBUFFER_BINDING),
                viewport,
                program: get(GL_CURRENT_PROGRAM),
                active_texture,
                texture_2d: get(GL_TEXTURE_BINDING_2D),
                texture_external: get(GL_TEXTURE_BINDING_EXTERNAL_OES),
                sampler: get(GL_SAMPLER_BINDING),
                array_buffer: get(GL_ARRAY_BUFFER_BINDING),
                unpack_buffer: get(GL_PIXEL_UNPACK_BUFFER_BINDING),
                vertex_array: get(GL_VERTEX_ARRAY_BINDING),
                enabled,
                color_mask,
            }
        }
    }

    unsafe fn restore(&self) {
        unsafe {
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, self.texture_2d as GLuint);
            glBindTexture(GL_TEXTURE_EXTERNAL_OES, self.texture_external as GLuint);
            glBindSampler(0, self.sampler as GLuint);
            glActiveTexture(self.active_texture as GLenum);
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, self.draw_framebuffer as GLuint);
            glBindFramebuffer(GL_READ_FRAMEBUFFER, self.read_framebuffer as GLuint);
            let [x, y, w, h] = self.viewport;
            glViewport(x, y, w, h);
            glUseProgram(self.program as GLuint);
            glBindVertexArray(self.vertex_array as GLuint);
            glBindBuffer(GL_ARRAY_BUFFER, self.array_buffer as GLuint);
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, self.unpack_buffer as GLuint);
            for (cap, on) in self.enabled {
                if on == GL_FALSE {
                    glDisable(cap);
                } else {
                    glEnable(cap);
                }
            }
            let [r, g, b, a] = self.color_mask;
            glColorMask(r, g, b, a);
        }
    }
}

/// Lives in the rendering notifier: the GL side, and the picture on screen
/// (kept until the next one is drawn, so the codec can't overwrite it
/// while the GPU still reads it).
struct Renderer {
    weak: slint::Weak<AppWindow>,
    gl: Option<Gl>,
    shown: Option<GpuPicture>,
    /// The stream view shows our texture (for this stream, at this size).
    image_set: Option<(u64, (u32, u32))>,
}

impl Renderer {
    fn setup(&mut self, api: &GraphicsAPI<'_>) {
        let gpu = GpuVideo::global();
        if !matches!(api, GraphicsAPI::NativeOpenGL { .. }) {
            info!("zero-copy video unavailable: the renderer doesn't use OpenGL");
            gpu.set_renderer_ready(false);
            return;
        }
        match Gl::new() {
            Ok(gl) => {
                self.gl = Some(gl);
                gpu.set_renderer_ready(true);
            }
            Err(e) => {
                info!("zero-copy video unavailable: {e}");
                gpu.set_renderer_ready(false);
            }
        }
    }

    fn before_rendering(&mut self) {
        let gpu = GpuVideo::global();
        // A stream ended (or a new one began): let go of the old picture.
        if self.shown.as_ref().is_some_and(|p| p.generation != gpu.generation()) {
            self.shown = None;
        }
        let Some(gl) = self.gl.as_mut() else { return };
        let Some(picture) = gpu.take() else { return };
        if picture.generation != gpu.generation() {
            return;
        }
        match gl.draw(&picture) {
            Ok((texture, resized)) => {
                let key = (picture.generation, (picture.width, picture.height));
                // The previous buffer goes back to the codec now.
                self.shown = Some(picture);
                if resized || self.image_set != Some(key) {
                    let Some(id) = NonZeroU32::new(texture) else { return };
                    // SAFETY: a live RGBA texture of this context, of that size.
                    let image = unsafe {
                        slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(id, (key.1.0, key.1.1).into())
                    }
                    .build();
                    if let Some(ui) = self.weak.upgrade() {
                        ui.set_stream_frame(image);
                    }
                    self.image_set = Some(key);
                }
            }
            Err(e) => {
                gpu.fail(e);
                gl.delete();
                self.gl = None;
                gpu.set_renderer_ready(false);
            }
        }
    }

    fn teardown(&mut self) {
        let gpu = GpuVideo::global();
        gpu.set_renderer_ready(false);
        self.shown = None;
        if let Some(mut gl) = self.gl.take() {
            gl.delete();
        }
        // The texture is gone with the context: don't leave it on screen.
        if self.image_set.take().is_some()
            && let Some(ui) = self.weak.upgrade()
        {
            ui.set_stream_frame(slint::Image::default());
        }
    }
}

/// Hooks the renderer into the window's rendering (Android).
pub fn install(ui: &AppWindow) {
    let mut renderer = Renderer {
        weak: ui.as_weak(),
        gl: None,
        shown: None,
        image_set: None,
    };
    let result = ui.window().set_rendering_notifier(move |state, api| match state {
        RenderingState::RenderingSetup => renderer.setup(api),
        RenderingState::BeforeRendering => renderer.before_rendering(),
        RenderingState::RenderingTeardown => renderer.teardown(),
        _ => {}
    });
    if let Err(e) = result {
        warn!("zero-copy video unavailable: {e:?}");
    }
}
