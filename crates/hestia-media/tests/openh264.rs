//! Decodes a real H.264 stream (x264, High profile, no B-frames, one slice
//! per frame, IDR every 3 frames with SPS/PPS repeated — the shape of a
//! Sunshine/Hermes stream) through the OpenH264 backend and `DecodingSink`.
//!
//! Fixture: left half red, right half blue, 64x36, BT.601 limited range:
//! `ffmpeg -f lavfi -i color=c=red:s=32x36 -f lavfi -i color=c=blue:s=32x36
//!  -filter_complex "[0][1]hstack,format=yuv420p" -frames:v 6 -c:v libx264
//!  -profile:v high -tune zerolatency -bf 0 -x264-params slices=1:keyint=3
//!  -f h264 red-blue-64x36-high.h264`
#![cfg(feature = "openh264")]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_media::software_h264::OpenH264Decoder;
use hestia_media::{Colorimetry, DecodingSink, FrameDecoder, DecodedFrame};
use hestia_stream::{EncodedFrame, VideoSink};

const CLIP: &[u8] = include_bytes!("data/red-blue-64x36-high.h264");

/// Splits Annex B into access units. With one slice per frame, any NAL that
/// follows a slice starts the next unit.
fn access_units(data: &[u8]) -> Vec<(Vec<u8>, bool)> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            nals.push((start, data[i + 3] & 0x1f));
            i += 3;
        } else {
            i += 1;
        }
    }
    let (mut units, mut unit_start, mut seen_slice, mut idr) = (Vec::new(), 0, false, false);
    for (pos, nal) in nals {
        if seen_slice {
            units.push((data[unit_start..pos].to_vec(), idr));
            (unit_start, seen_slice, idr) = (pos, false, false);
        }
        if nal == 1 || nal == 5 {
            seen_slice = true;
            idr |= nal == 5;
        }
    }
    if seen_slice {
        units.push((data[unit_start..].to_vec(), idr));
    }
    units
}

fn frame(n: u32, data: Vec<u8>, keyframe: bool) -> EncodedFrame {
    EncodedFrame {
        frame_number: n,
        keyframe,
        data,
        received_at: Instant::now(),
        idr_requested_at: None,
        host_latency: None,
    }
}

fn pixel(f: &DecodedFrame, x: usize, y: usize) -> [u8; 4] {
    let o = (y * f.width as usize + x) * 4;
    f.rgba().unwrap()[o..o + 4].try_into().unwrap()
}

fn near(a: [u8; 4], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= 6) && a[3] == 255
}

#[test]
fn splits_the_fixture() {
    let units = access_units(CLIP);
    assert_eq!(units.len(), 6);
    let keyframes: Vec<bool> = units.iter().map(|u| u.1).collect();
    assert_eq!(keyframes, [true, false, false, true, false, false]);
}

#[test]
fn decodes_high_profile_to_rgba() {
    let mut decoder = OpenH264Decoder::new(Colorimetry::Bt601Limited).expect("decoder");
    let mut out = DecodedFrame::default();
    for (n, (data, key)) in access_units(CLIP).into_iter().enumerate() {
        let got = decoder.decode(&frame(n as u32, data, key), &mut out).expect("decode");
        assert!(got, "frame {n} produced no picture (no-delay decoding)");
        assert_eq!((out.width, out.height), (64, 36), "cropped size");
        assert!(
            near(pixel(&out, 8, 18), [255, 0, 0]),
            "frame {n} left: {:?}",
            pixel(&out, 8, 18)
        );
        assert!(
            near(pixel(&out, 56, 18), [0, 0, 255]),
            "frame {n} right: {:?}",
            pixel(&out, 56, 18)
        );
        assert!(near(pixel(&out, 0, 35), [255, 0, 0]));
        assert!(near(pixel(&out, 63, 0), [0, 0, 255]));
    }
}

#[test]
fn decoding_sink_presents_every_frame() {
    let decoder = OpenH264Decoder::new(Colorimetry::default()).unwrap();
    let shown = Arc::new(Mutex::new(Vec::new()));
    let s = shown.clone();
    let mut sink = DecodingSink::new(
        Box::new(decoder),
        Box::new(move |f: &DecodedFrame| {
            s.lock()
                .unwrap()
                .push((f.frame_number, f.width, f.height, pixel(f, 8, 18)))
        }),
    );
    sink.setup(OpenH264Decoder::formats()[0], 64, 36, 30).unwrap();
    for (n, (data, key)) in access_units(CLIP).into_iter().enumerate() {
        sink.submit(frame(n as u32 + 1, data, key));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    sink.cleanup();
    let shown = shown.lock().unwrap();
    assert_eq!(shown.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
    assert!(shown.iter().all(|s| s.1 == 64 && s.2 == 36 && near(s.3, [255, 0, 0])));
    let stats = sink.stats();
    assert_eq!(stats.errors.load(std::sync::atomic::Ordering::Relaxed), 0);
}
