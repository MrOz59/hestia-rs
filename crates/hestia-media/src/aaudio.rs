//! Low-latency PCM output through Android AAudio.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use log::{info, warn};
use ndk_sys as aaudio;

use crate::audio_fifo::SAMPLE_RATE;
use crate::audio_playback::{PcmOutput, PcmReader};

#[derive(Default)]
pub struct AAudioOutput {
    stream: Option<Output>,
    reader: Option<PcmReader>,
    lost: Arc<AtomicBool>,
}

impl PcmOutput for AAudioOutput {
    fn start(&mut self, reader: PcmReader) -> Result<(), String> {
        self.stop();
        self.stream = Some(open_output(&reader, &self.lost)?);
        self.reader = Some(reader);
        Ok(())
    }

    fn maintain(&mut self) {
        if self.lost.swap(false, Ordering::Relaxed)
            && let Some(reader) = &self.reader
        {
            self.stream = None;
            match open_output(reader, &self.lost) {
                Ok(output) => self.stream = Some(output),
                Err(e) => warn!("reopening audio output failed: {e}"),
            }
        }
    }

    fn stop(&mut self) {
        self.stream = None;
        self.reader = None;
        self.lost.store(false, Ordering::Relaxed);
    }
}

struct CallbackState {
    reader: PcmReader,
    lost: Arc<AtomicBool>,
    channels: usize,
}

struct Output {
    stream: *mut aaudio::AAudioStream,
    _callback: Box<CallbackState>,
}

// SAFETY: the producer thread owns all stream operations. AAudio's callback
// only borrows the heap allocation and accesses synchronized state.
unsafe impl Send for Output {}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: the stream is live and close waits for its callbacks to finish.
        unsafe {
            aaudio::AAudioStream_requestStop(self.stream);
            aaudio::AAudioStream_close(self.stream);
        }
    }
}

type SetChannelMask = unsafe extern "C" fn(*mut aaudio::AAudioStreamBuilder, aaudio::aaudio_channel_mask_t);

/// Position masks are available from Android API 32. Load the symbol at
/// runtime to keep the same APK usable from API 26.
fn channel_mask_setter() -> Option<SetChannelMask> {
    static LIBRARY: OnceLock<Option<libloading::Library>> = OnceLock::new();
    let library = LIBRARY.get_or_init(|| {
        // SAFETY: this opens the system audio library already used by ndk-sys.
        unsafe { libloading::Library::new("libaaudio.so").ok() }
    });
    // SAFETY: the type matches the NDK declaration and the library stays loaded.
    unsafe {
        library
            .as_ref()?
            .get::<SetChannelMask>(b"AAudioStreamBuilder_setChannelMask\0")
            .ok()
            .map(|f| *f)
    }
}

fn open_output(reader: &PcmReader, lost: &Arc<AtomicBool>) -> Result<Output, String> {
    let requested = reader.channels();
    if requested > 2
        && let Some(set_mask) = channel_mask_setter()
    {
        for &(channels, mask) in &[(8, aaudio::AAUDIO_CHANNEL_7POINT1), (6, aaudio::AAUDIO_CHANNEL_5POINT1)] {
            if channels <= requested {
                match open_stream(
                    reader,
                    lost,
                    channels,
                    Some((set_mask, mask as aaudio::aaudio_channel_mask_t)),
                ) {
                    Ok(output) => return Ok(output),
                    Err(e) => info!("AAudio {channels} ch unavailable: {e}"),
                }
            }
        }
    }
    open_stream(reader, lost, 2, None)
}

fn open_stream(
    reader: &PcmReader,
    lost: &Arc<AtomicBool>,
    channels: usize,
    channel_mask: Option<(SetChannelMask, aaudio::aaudio_channel_mask_t)>,
) -> Result<Output, String> {
    let mut callback = Box::new(CallbackState {
        reader: reader.clone(),
        lost: lost.clone(),
        channels,
    });
    let userdata = ptr::from_mut(callback.as_mut()).cast();
    let mut builder = ptr::null_mut();
    let mut stream = ptr::null_mut();

    // SAFETY: the builder lives through open and is deleted afterward. The
    // boxed callback state outlives the stream, including failed starts.
    unsafe {
        check("create builder", aaudio::AAudio_createStreamBuilder(&mut builder))?;
        aaudio::AAudioStreamBuilder_setDirection(builder, aaudio::AAUDIO_DIRECTION_OUTPUT as _);
        aaudio::AAudioStreamBuilder_setFormat(builder, aaudio::AAUDIO_FORMAT_PCM_FLOAT as _);
        aaudio::AAudioStreamBuilder_setSampleRate(builder, SAMPLE_RATE as i32);
        aaudio::AAudioStreamBuilder_setSharingMode(builder, aaudio::AAUDIO_SHARING_MODE_SHARED as _);
        if let Some((set_mask, mask)) = channel_mask {
            set_mask(builder, mask);
            // Use the system mixer with a position mask for HDMI/TV routing.
            aaudio::AAudioStreamBuilder_setPerformanceMode(builder, aaudio::AAUDIO_PERFORMANCE_MODE_NONE as _);
        } else {
            aaudio::AAudioStreamBuilder_setChannelCount(builder, channels as i32);
            aaudio::AAudioStreamBuilder_setPerformanceMode(builder, aaudio::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY as _);
        }
        aaudio::AAudioStreamBuilder_setDataCallback(builder, Some(render), userdata);
        aaudio::AAudioStreamBuilder_setErrorCallback(builder, Some(disconnected), userdata);
        let status = aaudio::AAudioStreamBuilder_openStream(builder, &mut stream);
        aaudio::AAudioStreamBuilder_delete(builder);
        check("open", status)?;
        let output = Output {
            stream,
            _callback: callback,
        };
        if aaudio::AAudioStream_getChannelCount(stream) != channels as i32
            || aaudio::AAudioStream_getSampleRate(stream) != SAMPLE_RATE as i32
            || aaudio::AAudioStream_getFormat(stream) != aaudio::AAUDIO_FORMAT_PCM_FLOAT as i32
        {
            return Err("AAudio returned an unexpected PCM format".into());
        }
        check("start", aaudio::AAudioStream_requestStart(stream))?;
        info!(
            "audio output: AAudio {SAMPLE_RATE} Hz (host {} ch, stream {channels} ch, burst {} frames)",
            reader.channels(),
            aaudio::AAudioStream_getFramesPerBurst(stream)
        );
        Ok(output)
    }
}

fn check(operation: &str, status: aaudio::aaudio_result_t) -> Result<(), String> {
    if status == aaudio::AAUDIO_OK as i32 {
        Ok(())
    } else {
        Err(format!("AAudio {operation} failed ({status})"))
    }
}

unsafe extern "C" fn render(
    _: *mut aaudio::AAudioStream,
    userdata: *mut c_void,
    data: *mut c_void,
    frames: i32,
) -> aaudio::aaudio_data_callback_result_t {
    if frames > 0 {
        // SAFETY: AAudio supplies a valid callback state and a writable PCM
        // buffer whose format was checked before playback started.
        let state = unsafe { &*userdata.cast::<CallbackState>() };
        let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), frames as usize * state.channels) };
        state.reader.read(out, state.channels, |s| s.clamp(-1.0, 1.0), 0.0);
    }
    aaudio::AAUDIO_CALLBACK_RESULT_CONTINUE as _
}

unsafe extern "C" fn disconnected(_: *mut aaudio::AAudioStream, userdata: *mut c_void, error: aaudio::aaudio_result_t) {
    // SAFETY: AAudio holds this callback state alive until the stream closes.
    let state = unsafe { &*userdata.cast::<CallbackState>() };
    warn!("audio output lost: AAudio {error}");
    state.lost.store(true, Ordering::Relaxed);
}
