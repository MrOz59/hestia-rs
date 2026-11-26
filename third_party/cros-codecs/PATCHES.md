# Hestia RS changes to cros-codecs

Upstream: https://github.com/chromeos/cros-codecs at
`a050c6cde530e563197ee9e3e72ab01f48a0c41f` (the v0.0.6 code; upstream's
`Cargo.toml` there still says 0.0.5).

## Low delay for H.264 and H.265

Upstream's stateless H.264 and H.265 decoders hold pictures back in two ways
that cost a game stream one frame or more of latency:

* a picture is finished (submitted to the hardware) only when the first slice
  of the *next* picture arrives;
* a decoded picture waits in the DPB until the bumping process needs its
  buffer (H.264 C.4.5.3) or the reorder limits say so (H.265 C.5.2), which for
  an H.264 stream without VUI `bitstream_restriction` means until the DPB is
  full.

Game-streaming hosts send no B-frames, so output order is decoding order. Two
methods on `StatelessDecoder<H264, _>` and `StatelessDecoder<H265, _>`:

1. `set_low_delay(true)`: every finished picture goes to the ready queue at
   once (and stays in the DPB as a reference). H.264: `finish_picture` calls
   the new `Dpb::output_all` (`codec/h264/dpb.rs`). Before checking for space,
   `bump_as_needed` reclaims pictures that have been output and subsequently
   retired by adaptive reference marking. H.265: `finish_picture`
   bumps every picture still needed for output.
2. `end_access_unit()`: finishes the current picture now. The caller feeds a
   whole access unit, NAL by NAL, then calls it.

A `low_delay` field in `H264DecoderState` / `H265DecoderState` holds the mode
(off by default, so upstream behaviour is unchanged). Every change is marked
with a `Hestia RS` comment. Interlaced streams are not supported in low-delay
mode (the second field of a pair would be finished on its own).

## cros-libva with libva 2.23

libva 2.23 added fields to `VAEncPictureParameterBufferVP9`, and no
cros-libva release builds against it. `Cargo.toml` takes cros-libva from git
at `59384456ac2ae78c0c3e5515f41ef1efd9b802cf` (its fix, #37), whose
`EncPictureParameterBufferVP9::new` takes one more argument
(`seg_id_block_size`): the VP9 encoder
(`encoder/stateless/vp9/vaapi.rs`) passes 0. Hestia RS doesn't encode; this
only keeps the crate building.

## Packaging

`Cargo.toml`: version 0.0.6, no examples, tests or dev-dependencies (their
files are not copied), and `log` without `release_max_level_debug` (a feature
that would cap the whole application's logging in release builds).
`rustfmt.toml` is upstream's, so `cargo fmt --all` (which also checks local
path dependencies) holds this code to upstream's style, not the workspace's.

## Updating

Copy the new upstream `src/` (without the `test_data` directories), `LICENSE`
and `README.md`, reapply the changes above (search for `Hestia RS`), and take
the `cros-libva` version upstream uses once a release builds against current
libva.
