//! YUV to RGB on the GPU for the desktop renderer.
//!
//! A software decoder hands over 4:2:0 planes (I420, tightly packed). This
//! pass uploads them into three single-channel textures of the window's GL
//! context and draws one quad into an RGBA texture, the shader doing the
//! colour conversion, so the CPU neither converts nor copies RGBA and the
//! upload is 1.5 bytes per pixel instead of 4. The textures persist between
//! frames (no per-frame allocation, unlike an image handed to the toolkit).
//!
//! It needs OpenGL 3.0 or OpenGL ES 3.0 (single-channel `R8` textures,
//! vertex array objects). Every bit of state it touches is saved and put
//! back, since the toolkit's renderer caches GL state.
//!
//! Independent of Slint and of the decoder types: functions come from a
//! `get_proc_address` loader, pictures as plain slices.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

pub(crate) type GLenum = u32;
pub(crate) type GLuint = u32;
type GLint = i32;
type GLsizei = i32;
type GLboolean = u8;
type GLfloat = f32;
type GLsizeiptr = isize;

const GL_NO_ERROR: GLenum = 0;
const GL_FALSE: GLboolean = 0;
const GL_TRUE: GLboolean = 1;
const GL_TRIANGLE_STRIP: GLenum = 0x0005;
const GL_CULL_FACE: GLenum = 0x0B44;
const GL_DEPTH_TEST: GLenum = 0x0B71;
const GL_STENCIL_TEST: GLenum = 0x0B90;
const GL_VIEWPORT: GLenum = 0x0BA2;
const GL_BLEND: GLenum = 0x0BE2;
const GL_SCISSOR_TEST: GLenum = 0x0C11;
const GL_COLOR_WRITEMASK: GLenum = 0x0C23;
const GL_UNPACK_ROW_LENGTH: GLenum = 0x0CF2;
const GL_UNPACK_SKIP_ROWS: GLenum = 0x0CF3;
const GL_UNPACK_SKIP_PIXELS: GLenum = 0x0CF4;
const GL_UNPACK_ALIGNMENT: GLenum = 0x0CF5;
const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_FLOAT: GLenum = 0x1406;
const GL_RED: GLenum = 0x1903;
const GL_RGBA: GLenum = 0x1908;
const GL_VERSION: GLenum = 0x1F02;
const GL_LINEAR: GLint = 0x2601;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
const GL_RGBA8: GLint = 0x8058;
const GL_TEXTURE_BINDING_2D: GLenum = 0x8069;
const GL_CLAMP_TO_EDGE: GLint = 0x812F;
const GL_TEXTURE0: GLenum = 0x84C0;
const GL_ACTIVE_TEXTURE: GLenum = 0x84E0;
const GL_VERTEX_ARRAY_BINDING: GLenum = 0x85B5;
const GL_ARRAY_BUFFER: GLenum = 0x8892;
const GL_ARRAY_BUFFER_BINDING: GLenum = 0x8894;
const GL_STATIC_DRAW: GLenum = 0x88E4;
const GL_PIXEL_UNPACK_BUFFER: GLenum = 0x88EC;
const GL_PIXEL_UNPACK_BUFFER_BINDING: GLenum = 0x88EF;
const GL_SAMPLER_BINDING: GLenum = 0x8919;
const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
const GL_VERTEX_SHADER: GLenum = 0x8B31;
const GL_COMPILE_STATUS: GLenum = 0x8B81;
const GL_LINK_STATUS: GLenum = 0x8B82;
const GL_CURRENT_PROGRAM: GLenum = 0x8B8D;
const GL_R8: GLint = 0x8229;
const GL_DRAW_FRAMEBUFFER_BINDING: GLenum = 0x8CA6;
const GL_READ_FRAMEBUFFER: GLenum = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: GLenum = 0x8CA9;
const GL_READ_FRAMEBUFFER_BINDING: GLenum = 0x8CAA;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
pub(crate) const GL_FRAMEBUFFER: GLenum = 0x8D40;

macro_rules! gl_functions {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// The GL entry points the pass uses, loaded once per context.
        #[allow(non_snake_case)]
        pub(crate) struct Gl {
            $(pub(crate) $name: unsafe extern "system" fn($($arg),*) $(-> $ret)?,)*
            /// Sampler objects (GL 3.3, GLES 3.0): their unit bindings
            /// override texture parameters, so they are cleared and restored.
            glBindSampler: Option<unsafe extern "system" fn(GLuint, GLuint)>,
        }

        impl Gl {
            pub(crate) fn load(get: &dyn Fn(&CStr) -> *const c_void) -> Result<Self, String> {
                let find = |name: &str| {
                    let name = CStr::from_bytes_with_nul(name.as_bytes()).expect("NUL-terminated name");
                    let p = get(name);
                    // Some loaders return small integers for unknown names.
                    (p as usize > 8).then_some(p)
                };
                Ok(Self {
                    $($name: {
                        let p = find(concat!(stringify!($name), "\0"))
                            .ok_or(concat!(stringify!($name), " is unavailable"))?;
                        // SAFETY: the loader returned this entry point for this name.
                        unsafe { std::mem::transmute::<*const c_void, unsafe extern "system" fn($($arg),*) $(-> $ret)?>(p) }
                    },)*
                    // SAFETY: as above, for glBindSampler's signature.
                    glBindSampler: find("glBindSampler\0").map(|p| unsafe {
                        std::mem::transmute::<*const c_void, unsafe extern "system" fn(GLuint, GLuint)>(p)
                    }),
                })
            }
        }
    };
}

gl_functions! {
    glGetError: fn() -> GLenum;
    glGetString: fn(GLenum) -> *const u8;
    glGetIntegerv: fn(GLenum, *mut GLint);
    glGetBooleanv: fn(GLenum, *mut GLboolean);
    glIsEnabled: fn(GLenum) -> GLboolean;
    glEnable: fn(GLenum);
    glDisable: fn(GLenum);
    glColorMask: fn(GLboolean, GLboolean, GLboolean, GLboolean);
    glViewport: fn(GLint, GLint, GLsizei, GLsizei);
    glPixelStorei: fn(GLenum, GLint);
    glActiveTexture: fn(GLenum);
    glGenTextures: fn(GLsizei, *mut GLuint);
    glDeleteTextures: fn(GLsizei, *const GLuint);
    glBindTexture: fn(GLenum, GLuint);
    glTexParameteri: fn(GLenum, GLenum, GLint);
    glTexImage2D: fn(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, *const c_void);
    glTexSubImage2D: fn(GLenum, GLint, GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *const c_void);
    glGenFramebuffers: fn(GLsizei, *mut GLuint);
    glDeleteFramebuffers: fn(GLsizei, *const GLuint);
    glBindFramebuffer: fn(GLenum, GLuint);
    glFramebufferTexture2D: fn(GLenum, GLenum, GLenum, GLuint, GLint);
    glCheckFramebufferStatus: fn(GLenum) -> GLenum;
    glCreateShader: fn(GLenum) -> GLuint;
    glShaderSource: fn(GLuint, GLsizei, *const *const c_char, *const GLint);
    glCompileShader: fn(GLuint);
    glGetShaderiv: fn(GLuint, GLenum, *mut GLint);
    glGetShaderInfoLog: fn(GLuint, GLsizei, *mut GLsizei, *mut c_char);
    glDeleteShader: fn(GLuint);
    glCreateProgram: fn() -> GLuint;
    glAttachShader: fn(GLuint, GLuint);
    glBindAttribLocation: fn(GLuint, GLuint, *const c_char);
    glLinkProgram: fn(GLuint);
    glGetProgramiv: fn(GLuint, GLenum, *mut GLint);
    glGetProgramInfoLog: fn(GLuint, GLsizei, *mut GLsizei, *mut c_char);
    glDeleteProgram: fn(GLuint);
    glUseProgram: fn(GLuint);
    glGetUniformLocation: fn(GLuint, *const c_char) -> GLint;
    glUniform1i: fn(GLint, GLint);
    glUniform3f: fn(GLint, GLfloat, GLfloat, GLfloat);
    glUniformMatrix3fv: fn(GLint, GLsizei, GLboolean, *const GLfloat);
    glGenBuffers: fn(GLsizei, *mut GLuint);
    glDeleteBuffers: fn(GLsizei, *const GLuint);
    glBindBuffer: fn(GLenum, GLuint);
    glBufferData: fn(GLenum, GLsizeiptr, *const c_void, GLenum);
    glGenVertexArrays: fn(GLsizei, *mut GLuint);
    glDeleteVertexArrays: fn(GLsizei, *const GLuint);
    glBindVertexArray: fn(GLuint);
    glVertexAttribPointer: fn(GLuint, GLint, GLenum, GLboolean, GLsizei, *const c_void);
    glEnableVertexAttribArray: fn(GLuint);
    glDrawArrays: fn(GLenum, GLint, GLsizei);
    glFinish: fn();
}

/// One 4:2:0 picture: tightly packed planes (chroma rounded up for odd
/// sizes) and the conversion to apply.
pub(crate) struct Planes<'a> {
    pub(crate) y: &'a [u8],
    pub(crate) u: &'a [u8],
    pub(crate) v: &'a [u8],
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Column-major YUV → RGB matrix, applied after subtracting `offset`
    /// (both for values normalised to 0..1).
    pub(crate) matrix: [f32; 9],
    pub(crate) offset: [f32; 3],
}

const VERTEX_BODY: &str = "
in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}";

// Row 0 of the planes (the picture's top) lands in row 0 of the target
// texture: the top-left origin the toolkit expects of borrowed textures.
// Chroma is sampled bilinearly, which upsamples it more smoothly than the
// CPU path's sample repetition.
const FRAGMENT_BODY: &str = "
uniform sampler2D u_y;
uniform sampler2D u_u;
uniform sampler2D u_v;
uniform mat3 u_matrix;
uniform vec3 u_offset;
in vec2 v_uv;
out vec4 frag_color;
void main() {
    vec3 yuv = vec3(texture(u_y, v_uv).r, texture(u_u, v_uv).r, texture(u_v, v_uv).r) - u_offset;
    frag_color = vec4(clamp(u_matrix * yuv, 0.0, 1.0), 1.0);
}";

/// Whether `GL_VERSION` names OpenGL ES, and its major and minor version.
fn parse_version(version: &str) -> Option<(bool, u32, u32)> {
    let es = version.starts_with("OpenGL ES");
    version.split_whitespace().find_map(|w| {
        let mut parts = w.split('.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts
            .next()?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        Some((es, major, minor.parse::<u32>().unwrap_or(0)))
    })
}

/// The GLSL dialect for the current context, from its `GL_VERSION`.
fn shader_header(version: &str) -> Result<&'static str, String> {
    match parse_version(version) {
        Some((true, 3.., _)) => Ok("#version 300 es\nprecision highp float;\n"),
        Some((false, 4.., _) | (false, 3, 2..)) => Ok("#version 150\n"),
        Some((false, 3, 1)) => Ok("#version 140\n"),
        Some((false, 3, 0)) => Ok("#version 130\n"),
        _ => Err(format!("needs OpenGL 3 or OpenGL ES 3, the context is {version:?}")),
    }
}

/// Sampler objects are core in OpenGL ES 3.0 and OpenGL 3.3. Older desktop
/// contexts may still hand out an entry point for them (GLX and EGL return
/// one for any name), so the version decides.
fn has_samplers(version: &str) -> bool {
    matches!(
        parse_version(version),
        Some((true, 3.., _) | (false, 4.., _) | (false, 3, 3..))
    )
}

#[derive(Default)]
struct Objects {
    program: GLuint,
    u_matrix: GLint,
    u_offset: GLint,
    vbo: GLuint,
    vao: GLuint,
    /// Y, U, V.
    planes: [GLuint; 3],
    fbo: GLuint,
    target: GLuint,
    size: (u32, u32),
}

/// The conversion pass of one GL context.
pub(crate) struct YuvPass {
    gl: Gl,
    header: &'static str,
    pub(crate) version: String,
    objects: Option<Objects>,
}

impl YuvPass {
    /// Loads the entry points and checks the context; the context must be
    /// current.
    pub(crate) fn new(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Result<Self, String> {
        let mut gl = Gl::load(get_proc_address)?;
        // SAFETY: a context is current; the string is static.
        let version = unsafe {
            let s = (gl.glGetString)(GL_VERSION);
            if s.is_null() {
                return Err("no current OpenGL context".into());
            }
            CStr::from_ptr(s.cast()).to_string_lossy().into_owned()
        };
        let header = shader_header(&version)?;
        if !has_samplers(&version) {
            gl.glBindSampler = None;
        }
        Ok(Self {
            gl,
            header,
            version,
            objects: None,
        })
    }

    /// Converts `planes` into the RGBA target texture. Returns the texture
    /// and whether it was (re)allocated (a borrowed image of it must be
    /// recreated). The context must be current.
    pub(crate) fn draw(&mut self, planes: &Planes<'_>) -> Result<(GLuint, bool), String> {
        let (w, h) = (planes.width as usize, planes.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if w == 0 || h == 0 || planes.y.len() < w * h || planes.u.len() < cw * ch || planes.v.len() < cw * ch {
            return Err(format!("short planes for a {w}x{h} picture"));
        }
        // SAFETY: the context is current; every pointer passed below is
        // either null or into the slices checked above.
        unsafe {
            let gl = &self.gl;
            // Earlier errors are the toolkit's business, not this pass's.
            for _ in 0..16 {
                if (gl.glGetError)() == GL_NO_ERROR {
                    break;
                }
            }
            let saved = SavedState::save(gl);
            let result = self.draw_saved(planes);
            saved.restore(&self.gl);
            result
        }
    }

    unsafe fn draw_saved(&mut self, planes: &Planes<'_>) -> Result<(GLuint, bool), String> {
        unsafe {
            if self.objects.is_none() {
                self.objects = Some(create_objects(&self.gl, self.header)?);
            }
            let (gl, Some(o)) = (&self.gl, self.objects.as_mut()) else {
                return Err("no GL objects".into());
            };
            let size = (planes.width, planes.height);
            let (w, h) = (size.0 as GLsizei, size.1 as GLsizei);
            let (cw, ch) = ((size.0.div_ceil(2)) as GLsizei, (size.1.div_ceil(2)) as GLsizei);
            (gl.glBindBuffer)(GL_PIXEL_UNPACK_BUFFER, 0);
            for (name, value) in [
                (GL_UNPACK_ALIGNMENT, 1),
                (GL_UNPACK_ROW_LENGTH, 0),
                (GL_UNPACK_SKIP_ROWS, 0),
                (GL_UNPACK_SKIP_PIXELS, 0),
            ] {
                (gl.glPixelStorei)(name, value);
            }
            let resized = o.size != size;
            if resized {
                for (i, (pw, ph)) in [(w, h), (cw, ch), (cw, ch)].into_iter().enumerate() {
                    (gl.glActiveTexture)(GL_TEXTURE0 + i as GLenum);
                    (gl.glBindTexture)(GL_TEXTURE_2D, o.planes[i]);
                    (gl.glTexImage2D)(
                        GL_TEXTURE_2D,
                        0,
                        GL_R8,
                        pw,
                        ph,
                        0,
                        GL_RED,
                        GL_UNSIGNED_BYTE,
                        ptr::null(),
                    );
                }
                (gl.glActiveTexture)(GL_TEXTURE0);
                (gl.glBindTexture)(GL_TEXTURE_2D, o.target);
                (gl.glTexImage2D)(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA8,
                    w,
                    h,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    ptr::null(),
                );
                (gl.glBindFramebuffer)(GL_FRAMEBUFFER, o.fbo);
                (gl.glFramebufferTexture2D)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, o.target, 0);
                let status = (gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER);
                if status != GL_FRAMEBUFFER_COMPLETE {
                    return Err(format!("video framebuffer incomplete ({status:#x})"));
                }
                o.size = size;
            }
            for (i, ((pw, ph), data)) in [(w, h), (cw, ch), (cw, ch)]
                .into_iter()
                .zip([planes.y, planes.u, planes.v])
                .enumerate()
            {
                (gl.glActiveTexture)(GL_TEXTURE0 + i as GLenum);
                if let Some(bind_sampler) = gl.glBindSampler {
                    bind_sampler(i as GLuint, 0);
                }
                (gl.glBindTexture)(GL_TEXTURE_2D, o.planes[i]);
                (gl.glTexSubImage2D)(
                    GL_TEXTURE_2D,
                    0,
                    0,
                    0,
                    pw,
                    ph,
                    GL_RED,
                    GL_UNSIGNED_BYTE,
                    data.as_ptr().cast(),
                );
            }
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, o.fbo);
            (gl.glViewport)(0, 0, w, h);
            for cap in DISABLED_CAPS {
                (gl.glDisable)(cap);
            }
            (gl.glColorMask)(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
            (gl.glUseProgram)(o.program);
            (gl.glUniformMatrix3fv)(o.u_matrix, 1, GL_FALSE, planes.matrix.as_ptr());
            let [y, u, v] = planes.offset;
            (gl.glUniform3f)(o.u_offset, y, u, v);
            (gl.glBindVertexArray)(o.vao);
            (gl.glDrawArrays)(GL_TRIANGLE_STRIP, 0, 4);
            match (gl.glGetError)() {
                GL_NO_ERROR => Ok((o.target, resized)),
                e => Err(format!("GL error {e:#x} drawing the video")),
            }
        }
    }

    /// Waits for submitted GL work (after a failure, before giving up).
    pub(crate) fn finish(&self) {
        // SAFETY: the context is current.
        unsafe { (self.gl.glFinish)() }
    }

    /// The framebuffer holding the converted picture (for tests reading
    /// it back).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn framebuffer(&self) -> Option<GLuint> {
        self.objects.as_ref().map(|o| o.fbo)
    }

    /// Frees the GL objects; the context must be current.
    pub(crate) fn delete(&mut self) {
        let Some(o) = self.objects.take() else { return };
        let gl = &self.gl;
        // SAFETY: names created in this context.
        unsafe {
            (gl.glDeleteProgram)(o.program);
            (gl.glDeleteBuffers)(1, &o.vbo);
            (gl.glDeleteVertexArrays)(1, &o.vao);
            (gl.glDeleteTextures)(3, o.planes.as_ptr());
            (gl.glDeleteTextures)(1, &o.target);
            (gl.glDeleteFramebuffers)(1, &o.fbo);
        }
    }
}

const DISABLED_CAPS: [GLenum; 5] = [GL_BLEND, GL_SCISSOR_TEST, GL_DEPTH_TEST, GL_STENCIL_TEST, GL_CULL_FACE];

unsafe fn compile(gl: &Gl, kind: GLenum, header: &str, body: &str) -> Result<GLuint, String> {
    unsafe {
        let shader = (gl.glCreateShader)(kind);
        let sources = [header.as_ptr().cast::<c_char>(), body.as_ptr().cast()];
        let lengths = [header.len() as GLint, body.len() as GLint];
        (gl.glShaderSource)(shader, 2, sources.as_ptr(), lengths.as_ptr());
        (gl.glCompileShader)(shader);
        let mut ok = 0;
        (gl.glGetShaderiv)(shader, GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = [0 as c_char; 512];
            (gl.glGetShaderInfoLog)(shader, log.len() as GLsizei, ptr::null_mut(), log.as_mut_ptr());
            (gl.glDeleteShader)(shader);
            return Err(format!(
                "video shader: {}",
                CStr::from_ptr(log.as_ptr()).to_string_lossy()
            ));
        }
        Ok(shader)
    }
}

/// Program, quad and textures; the bindings it changes are restored by the
/// caller's saved state.
unsafe fn create_objects(gl: &Gl, header: &str) -> Result<Objects, String> {
    unsafe {
        let vs = compile(gl, GL_VERTEX_SHADER, header, VERTEX_BODY)?;
        let fs = match compile(gl, GL_FRAGMENT_SHADER, header, FRAGMENT_BODY) {
            Ok(fs) => fs,
            Err(e) => {
                (gl.glDeleteShader)(vs);
                return Err(e);
            }
        };
        let program = (gl.glCreateProgram)();
        (gl.glAttachShader)(program, vs);
        (gl.glAttachShader)(program, fs);
        (gl.glBindAttribLocation)(program, 0, c"a_pos".as_ptr());
        (gl.glLinkProgram)(program);
        (gl.glDeleteShader)(vs);
        (gl.glDeleteShader)(fs);
        let mut ok = 0;
        (gl.glGetProgramiv)(program, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut log = [0 as c_char; 512];
            (gl.glGetProgramInfoLog)(program, log.len() as GLsizei, ptr::null_mut(), log.as_mut_ptr());
            (gl.glDeleteProgram)(program);
            return Err(format!(
                "video program: {}",
                CStr::from_ptr(log.as_ptr()).to_string_lossy()
            ));
        }
        let mut o = Objects {
            program,
            u_matrix: (gl.glGetUniformLocation)(program, c"u_matrix".as_ptr()),
            u_offset: (gl.glGetUniformLocation)(program, c"u_offset".as_ptr()),
            ..Objects::default()
        };
        // Sampler uniforms are program state: set once.
        (gl.glUseProgram)(program);
        for (unit, name) in [c"u_y", c"u_u", c"u_v"].into_iter().enumerate() {
            (gl.glUniform1i)((gl.glGetUniformLocation)(program, name.as_ptr()), unit as GLint);
        }
        // A full-screen quad, in its own vertex array.
        (gl.glGenVertexArrays)(1, &mut o.vao);
        (gl.glBindVertexArray)(o.vao);
        (gl.glGenBuffers)(1, &mut o.vbo);
        (gl.glBindBuffer)(GL_ARRAY_BUFFER, o.vbo);
        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        (gl.glBufferData)(
            GL_ARRAY_BUFFER,
            std::mem::size_of_val(&quad) as GLsizeiptr,
            quad.as_ptr().cast(),
            GL_STATIC_DRAW,
        );
        (gl.glVertexAttribPointer)(0, 2, GL_FLOAT, GL_FALSE, 0, ptr::null());
        (gl.glEnableVertexAttribArray)(0);

        (gl.glGenTextures)(3, o.planes.as_mut_ptr());
        (gl.glGenTextures)(1, &mut o.target);
        (gl.glActiveTexture)(GL_TEXTURE0);
        for texture in o.planes.into_iter().chain([o.target]) {
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            for (name, value) in [
                (GL_TEXTURE_MIN_FILTER, GL_LINEAR),
                (GL_TEXTURE_MAG_FILTER, GL_LINEAR),
                (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE),
                (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE),
            ] {
                (gl.glTexParameteri)(GL_TEXTURE_2D, name, value);
            }
        }
        (gl.glGenFramebuffers)(1, &mut o.fbo);
        match (gl.glGetError)() {
            GL_NO_ERROR => Ok(o),
            e => Err(format!("GL error {e:#x} setting up the video pass")),
        }
    }
}

/// The GL state the pass changes, as the toolkit left it.
struct SavedState {
    draw_framebuffer: GLint,
    read_framebuffer: GLint,
    viewport: [GLint; 4],
    program: GLint,
    active_texture: GLint,
    /// Texture and sampler bindings of units 0..3.
    textures: [GLint; 3],
    samplers: [GLint; 3],
    array_buffer: GLint,
    unpack_buffer: GLint,
    vertex_array: GLint,
    unpack: [(GLenum, GLint); 4],
    enabled: [(GLenum, GLboolean); 5],
    color_mask: [GLboolean; 4],
}

impl SavedState {
    unsafe fn save(gl: &Gl) -> Self {
        unsafe {
            let get = |name| {
                let mut v = 0;
                (gl.glGetIntegerv)(name, &mut v);
                v
            };
            let active_texture = get(GL_ACTIVE_TEXTURE);
            let mut textures = [0; 3];
            let mut samplers = [0; 3];
            for unit in 0..3 {
                (gl.glActiveTexture)(GL_TEXTURE0 + unit as GLenum);
                textures[unit] = get(GL_TEXTURE_BINDING_2D);
                if gl.glBindSampler.is_some() {
                    samplers[unit] = get(GL_SAMPLER_BINDING);
                }
            }
            let mut viewport = [0; 4];
            (gl.glGetIntegerv)(GL_VIEWPORT, viewport.as_mut_ptr());
            let mut color_mask = [GL_TRUE; 4];
            (gl.glGetBooleanv)(GL_COLOR_WRITEMASK, color_mask.as_mut_ptr());
            Self {
                draw_framebuffer: get(GL_DRAW_FRAMEBUFFER_BINDING),
                read_framebuffer: get(GL_READ_FRAMEBUFFER_BINDING),
                viewport,
                program: get(GL_CURRENT_PROGRAM),
                active_texture,
                textures,
                samplers,
                array_buffer: get(GL_ARRAY_BUFFER_BINDING),
                unpack_buffer: get(GL_PIXEL_UNPACK_BUFFER_BINDING),
                vertex_array: get(GL_VERTEX_ARRAY_BINDING),
                unpack: [
                    GL_UNPACK_ALIGNMENT,
                    GL_UNPACK_ROW_LENGTH,
                    GL_UNPACK_SKIP_ROWS,
                    GL_UNPACK_SKIP_PIXELS,
                ]
                .map(|name| (name, get(name))),
                enabled: DISABLED_CAPS.map(|cap| (cap, (gl.glIsEnabled)(cap))),
                color_mask,
            }
        }
    }

    unsafe fn restore(&self, gl: &Gl) {
        unsafe {
            for unit in 0..3 {
                (gl.glActiveTexture)(GL_TEXTURE0 + unit as GLenum);
                (gl.glBindTexture)(GL_TEXTURE_2D, self.textures[unit] as GLuint);
                if let Some(bind_sampler) = gl.glBindSampler {
                    bind_sampler(unit as GLuint, self.samplers[unit] as GLuint);
                }
            }
            (gl.glActiveTexture)(self.active_texture as GLenum);
            (gl.glBindFramebuffer)(GL_DRAW_FRAMEBUFFER, self.draw_framebuffer as GLuint);
            (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, self.read_framebuffer as GLuint);
            let [x, y, w, h] = self.viewport;
            (gl.glViewport)(x, y, w, h);
            (gl.glUseProgram)(self.program as GLuint);
            (gl.glBindVertexArray)(self.vertex_array as GLuint);
            (gl.glBindBuffer)(GL_ARRAY_BUFFER, self.array_buffer as GLuint);
            (gl.glBindBuffer)(GL_PIXEL_UNPACK_BUFFER, self.unpack_buffer as GLuint);
            for (name, value) in self.unpack {
                (gl.glPixelStorei)(name, value);
            }
            for (cap, on) in self.enabled {
                if on == GL_FALSE {
                    (gl.glDisable)(cap);
                } else {
                    (gl.glEnable)(cap);
                }
            }
            let [r, g, b, a] = self.color_mask;
            (gl.glColorMask)(r, g, b, a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{has_samplers, shader_header};

    #[test]
    fn picks_a_glsl_dialect_per_context() {
        assert!(shader_header("OpenGL ES 3.2 Mesa 25.2.8").unwrap().contains("300 es"));
        assert!(shader_header("OpenGL ES 3.0 V@0502.0").unwrap().contains("300 es"));
        assert!(shader_header("4.6 (Core Profile) Mesa 25.2.8").unwrap().contains("150"));
        assert!(shader_header("4.6.0 NVIDIA 580.95.05").unwrap().contains("150"));
        assert!(shader_header("3.1 Mesa 21.0").unwrap().contains("140"));
        assert!(shader_header("3.0 Mesa 10.0").unwrap().contains("130"));
        assert!(shader_header("OpenGL ES 2.0 Mesa").is_err());
        assert!(shader_header("2.1 Mesa 20.0").is_err());
        assert!(shader_header("").is_err());
    }

    #[test]
    fn uses_sampler_objects_only_where_they_are_core() {
        assert!(has_samplers("OpenGL ES 3.0 V@0502.0"));
        assert!(has_samplers("3.3 (Core Profile) Mesa 25.2.8"));
        assert!(has_samplers("4.6.0 NVIDIA 580.95.05"));
        assert!(!has_samplers("3.1 Mesa 21.0"));
        assert!(!has_samplers("3.0 Mesa 10.0"));
    }
}

/// Runs the pass in a headless Mesa context (EGL, surfaceless platform),
/// loaded at run time so the build needs no EGL development files. Skips
/// when the machine has no EGL or no usable context.
#[cfg(all(test, target_os = "linux"))]
mod gl_tests {
    use std::ffi::{CStr, c_void};
    use std::ptr;

    use super::*;

    type Display = *mut c_void;
    type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *const c_void;
    type GetPlatformDisplay = unsafe extern "C" fn(GLenum, *mut c_void, *const isize) -> Display;
    type Initialize = unsafe extern "C" fn(Display, *mut i32, *mut i32) -> u32;
    type BindApi = unsafe extern "C" fn(u32) -> u32;
    type ChooseConfig = unsafe extern "C" fn(Display, *const i32, *mut *mut c_void, i32, *mut i32) -> u32;
    type CreateContext = unsafe extern "C" fn(Display, *mut c_void, *mut c_void, *const i32) -> *mut c_void;
    type MakeCurrent = unsafe extern "C" fn(Display, *mut c_void, *mut c_void, *mut c_void) -> u32;
    type DestroyContext = unsafe extern "C" fn(Display, *mut c_void) -> u32;
    type ReadPixels = unsafe extern "system" fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut c_void);

    const EGL_PLATFORM_SURFACELESS_MESA: GLenum = 0x31DD;
    const EGL_OPENGL_ES_API: u32 = 0x30A0;
    const EGL_OPENGL_API: u32 = 0x30A2;

    struct Egl {
        get_proc_address: GetProcAddress,
        display: Display,
        make_current: MakeCurrent,
        destroy_context: DestroyContext,
        create_context: CreateContext,
        bind_api: BindApi,
        choose_config: ChooseConfig,
    }

    unsafe fn symbol<T: Copy>(lib: *mut c_void, name: &CStr) -> Option<T> {
        let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
        (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }

    fn egl() -> Option<Egl> {
        unsafe {
            let lib = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return None;
            }
            let get_proc_address: GetProcAddress = symbol(lib, c"eglGetProcAddress")?;
            let platform_display = get_proc_address(c"eglGetPlatformDisplayEXT".as_ptr());
            if platform_display.is_null() {
                return None;
            }
            let platform_display: GetPlatformDisplay = std::mem::transmute(platform_display);
            let display = platform_display(EGL_PLATFORM_SURFACELESS_MESA, ptr::null_mut(), ptr::null());
            let initialize: Initialize = symbol(lib, c"eglInitialize")?;
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || initialize(display, &mut major, &mut minor) == 0 {
                return None;
            }
            Some(Egl {
                get_proc_address,
                display,
                make_current: symbol(lib, c"eglMakeCurrent")?,
                destroy_context: symbol(lib, c"eglDestroyContext")?,
                create_context: symbol(lib, c"eglCreateContext")?,
                bind_api: symbol(lib, c"eglBindAPI")?,
                choose_config: symbol(lib, c"eglChooseConfig")?,
            })
        }
    }

    /// Makes a context current: (api, renderable type bit, context attributes).
    fn context(egl: &Egl, api: u32, renderable: i32, attributes: &[i32]) -> Option<*mut c_void> {
        unsafe {
            if (egl.bind_api)(api) == 0 {
                return None;
            }
            // Renderable type, pbuffer surfaces (none is created: surfaceless).
            let config_attributes = [0x3040, renderable, 0x3033, 0x1, 0x3038];
            let (mut config, mut count) = (ptr::null_mut(), 0);
            if (egl.choose_config)(egl.display, config_attributes.as_ptr(), &mut config, 1, &mut count) == 0
                || count != 1
            {
                return None;
            }
            let context = (egl.create_context)(egl.display, config, ptr::null_mut(), attributes.as_ptr());
            if context.is_null() || (egl.make_current)(egl.display, ptr::null_mut(), ptr::null_mut(), context) == 0 {
                return None;
            }
            Some(context)
        }
    }

    /// Converts a picture whose chroma is constant per run (so bilinear
    /// chroma upsampling cannot blur it) and compares with the matrix.
    fn check_context(egl: &Egl, label: &str) {
        let get = |name: &CStr| unsafe { (egl.get_proc_address)(name.as_ptr()) };
        let mut pass = YuvPass::new(&get).unwrap_or_else(|e| panic!("{label}: {e}"));
        let read: ReadPixels = unsafe { std::mem::transmute(get(c"glReadPixels")) };
        let gl = Gl::load(&get).unwrap();
        // Bindings and settings the toolkit would have.
        let mut texture = 0;
        unsafe {
            (gl.glGenTextures)(1, &mut texture);
            (gl.glActiveTexture)(GL_TEXTURE0 + 1);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            (gl.glActiveTexture)(GL_TEXTURE0);
            (gl.glEnable)(GL_BLEND);
            (gl.glViewport)(3, 4, 50, 60);
            (gl.glPixelStorei)(GL_UNPACK_ALIGNMENT, 4);
        }
        let state = || unsafe {
            let get = |name| {
                let mut v = [0; 4];
                (gl.glGetIntegerv)(name, v.as_mut_ptr());
                v
            };
            (gl.glActiveTexture)(GL_TEXTURE0 + 1);
            let unit1 = get(GL_TEXTURE_BINDING_2D);
            (gl.glActiveTexture)(GL_TEXTURE0);
            (
                unit1,
                get(GL_VIEWPORT),
                get(GL_UNPACK_ALIGNMENT),
                (gl.glIsEnabled)(GL_BLEND),
                get(GL_DRAW_FRAMEBUFFER_BINDING),
                get(GL_CURRENT_PROGRAM),
            )
        };
        let before = state();

        let (w, h) = (63usize, 35usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y: Vec<u8> = (0..w * h).map(|i| (16 + (i * 37) % 220) as u8).collect();
        // BT.601 limited range, as the CPU path's default.
        let (kr, kb, kg) = (0.299f32, 0.114f32, 0.587f32);
        let (ys, cs) = (255.0 / 219.0, 255.0 / 224.0);
        let (rv, bu) = (2.0 * (1.0 - kr), 2.0 * (1.0 - kb));
        let matrix = [
            ys,
            ys,
            ys,
            0.0,
            -bu * kb / kg * cs,
            bu * cs,
            rv * cs,
            -rv * kr / kg * cs,
            0.0,
        ];
        let offset = [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0];
        let mut out = vec![0u8; w * h * 4];
        let mut first = true;
        for (cu, cv) in [(128u8, 128u8), (16, 240), (240, 16), (54, 34)] {
            let (u, v) = (vec![cu; cw * ch], vec![cv; cw * ch]);
            let planes = Planes {
                y: &y,
                u: &u,
                v: &v,
                width: w as u32,
                height: h as u32,
                matrix,
                offset,
            };
            let (target, resized) = pass.draw(&planes).unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(target != 0);
            assert_eq!(resized, first, "{label}: textures are allocated once per size");
            first = false;
            assert_eq!(state(), before, "{label}: GL state restored");
            unsafe {
                (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, pass.framebuffer().unwrap());
                read(
                    0,
                    0,
                    w as GLsizei,
                    h as GLsizei,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    out.as_mut_ptr().cast(),
                );
                (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, 0);
            }
            for (i, px) in out.chunks_exact(4).enumerate() {
                let yuv = [
                    f32::from(y[i]) / 255.0 - offset[0],
                    f32::from(cu) / 255.0 - offset[1],
                    f32::from(cv) / 255.0 - offset[2],
                ];
                for c in 0..3 {
                    let want = (matrix[c] * yuv[0] + matrix[3 + c] * yuv[1] + matrix[6 + c] * yuv[2]).clamp(0.0, 1.0);
                    let got = f32::from(px[c]);
                    assert!(
                        (want * 255.0 - got).abs() <= 1.0,
                        "{label}: pixel {i} channel {c}: {got} vs {}",
                        want * 255.0
                    );
                }
                assert_eq!(px[3], 255);
            }
        }
        // Row 0 of the planes is row 0 of the target (top-left origin).
        let mut y = vec![16u8; w * h];
        y[..w].fill(235);
        let (u, v) = (vec![128u8; cw * ch], vec![128u8; cw * ch]);
        let planes = Planes {
            y: &y,
            u: &u,
            v: &v,
            width: w as u32,
            height: h as u32,
            matrix,
            offset,
        };
        pass.draw(&planes).unwrap();
        unsafe {
            (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, pass.framebuffer().unwrap());
            read(
                0,
                0,
                w as GLsizei,
                h as GLsizei,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                out.as_mut_ptr().cast(),
            );
            (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, 0);
        }
        assert!(out[0] > 250 && out[(h - 1) * w * 4] < 5, "{label}: picture upside down");
        assert!(
            pass.draw(&Planes { y: &y[..10], ..planes }).is_err(),
            "short planes are refused"
        );
        pass.delete();
        unsafe { (gl.glDeleteTextures)(1, &texture) };
    }

    #[test]
    fn converts_on_mesa_in_every_context_kind() {
        let Some(egl) = egl() else {
            eprintln!("skipped: no EGL surfaceless platform on this machine");
            return;
        };
        const MAJOR: i32 = 0x3098;
        const MINOR: i32 = 0x30FB;
        const PROFILE: i32 = 0x30FD;
        const NONE: i32 = 0x3038;
        let mut tested = 0;
        for (label, api, renderable, attributes) in [
            // Slint's FemtoVG renderer asks for OpenGL ES 2.0 and gets 3.x.
            ("OpenGL ES 2.0 request", EGL_OPENGL_ES_API, 0x4, &[MAJOR, 2, NONE][..]),
            (
                "OpenGL ES 3.0",
                EGL_OPENGL_ES_API,
                0x40,
                &[MAJOR, 3, MINOR, 0, NONE][..],
            ),
            (
                "OpenGL 3.3 core",
                EGL_OPENGL_API,
                0x8,
                &[MAJOR, 3, MINOR, 3, PROFILE, 1, NONE][..],
            ),
            ("OpenGL compatibility", EGL_OPENGL_API, 0x8, &[NONE][..]),
        ] {
            let Some(context) = context(&egl, api, renderable, attributes) else {
                eprintln!("{label}: no such context here");
                continue;
            };
            let version = unsafe {
                let gl = Gl::load(&|name: &CStr| (egl.get_proc_address)(name.as_ptr())).unwrap();
                CStr::from_ptr((gl.glGetString)(GL_VERSION).cast())
                    .to_string_lossy()
                    .into_owned()
            };
            if shader_header(&version).is_ok() {
                check_context(&egl, label);
                tested += 1;
            } else {
                eprintln!("{label}: {version} is too old for the pass");
            }
            unsafe {
                (egl.make_current)(egl.display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                (egl.destroy_context)(egl.display, context);
            }
        }
        eprintln!("GPU colour pass checked in {tested} contexts");
    }
}
