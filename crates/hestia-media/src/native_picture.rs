//! Owned, platform-specific video buffers carried inline with a decoded frame.

/// The decoder transfers ownership to the renderer. Dropping an unused
/// picture releases it through its platform backend.
pub enum NativePicture {
    #[cfg(target_os = "android")]
    Android(crate::gpu_video::GpuPicture),
}
