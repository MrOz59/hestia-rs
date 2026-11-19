//! `ServerCodecModeSupport` flags reported in `/serverinfo`.

pub const SCM_H264: u32 = 0x0000_0001;
pub const SCM_HEVC: u32 = 0x0000_0100;
pub const SCM_HEVC_MAIN10: u32 = 0x0000_0200;
pub const SCM_AV1_MAIN8: u32 = 0x0001_0000;
pub const SCM_AV1_MAIN10: u32 = 0x0002_0000;
pub const SCM_H264_HIGH8_444: u32 = 0x0004_0000;
pub const SCM_HEVC_REXT8_444: u32 = 0x0008_0000;
pub const SCM_HEVC_REXT10_444: u32 = 0x0010_0000;
pub const SCM_AV1_HIGH8_444: u32 = 0x0020_0000;
pub const SCM_AV1_HIGH10_444: u32 = 0x0040_0000;
/// Hermes extension: PyroWave intra-only wavelet codec.
pub const SCM_PYROWAVE: u32 = 0x4000_0000;

pub const SCM_MASK_H264: u32 = SCM_H264 | SCM_H264_HIGH8_444;
pub const SCM_MASK_HEVC: u32 = SCM_HEVC | SCM_HEVC_MAIN10 | SCM_HEVC_REXT8_444 | SCM_HEVC_REXT10_444;
pub const SCM_MASK_AV1: u32 = SCM_AV1_MAIN8 | SCM_AV1_MAIN10 | SCM_AV1_HIGH8_444 | SCM_AV1_HIGH10_444;
pub const SCM_MASK_10BIT: u32 = SCM_HEVC_MAIN10 | SCM_HEVC_REXT10_444 | SCM_AV1_MAIN10 | SCM_AV1_HIGH10_444;

/// Human-readable summary of a codec mask, e.g. `["H.264", "HEVC", "HEVC 10-bit", "AV1"]`.
pub fn describe(mask: u32) -> Vec<&'static str> {
    let mut out = Vec::new();
    if mask & SCM_MASK_H264 != 0 {
        out.push("H.264");
    }
    if mask & SCM_HEVC != 0 {
        out.push("HEVC");
    }
    if mask & (SCM_HEVC_MAIN10 | SCM_HEVC_REXT10_444) != 0 {
        out.push("HEVC 10-bit");
    }
    if mask & (SCM_AV1_MAIN8 | SCM_AV1_HIGH8_444) != 0 {
        out.push("AV1");
    }
    if mask & (SCM_AV1_MAIN10 | SCM_AV1_HIGH10_444) != 0 {
        out.push("AV1 10-bit");
    }
    if mask & (SCM_H264_HIGH8_444 | SCM_HEVC_REXT8_444 | SCM_HEVC_REXT10_444 | SCM_AV1_HIGH8_444 | SCM_AV1_HIGH10_444)
        != 0
    {
        out.push("YUV 4:4:4");
    }
    if mask & SCM_PYROWAVE != 0 {
        out.push("PyroWave");
    }
    out
}
