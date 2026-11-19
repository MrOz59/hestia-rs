//! Mapping between core types and the Slint view models.

use hestia_core::nvhttp::{AppInfo, codecs};
use hestia_core::{AudioLayout, CodecPreference, HostRecord, HostSnapshot, LaunchMode, StreamSettings};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use crate::{AppCard, AppWindow, HostCard};

pub const RESOLUTIONS: [(u32, u32); 4] = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];
pub const FRAME_RATES: [u32; 5] = [30, 60, 90, 120, 144];
pub const AUDIO_LAYOUTS: [AudioLayout; 3] = [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71];

pub fn host_card(record: &HostRecord, snapshot: Option<&HostSnapshot>, checking: bool) -> HostCard {
    let online = snapshot.is_some_and(|s| s.online);
    let paired = snapshot.map_or(
        record.is_paired(),
        |s| if s.online { s.paired } else { record.is_paired() },
    );
    let caps = snapshot.and_then(|s| s.capabilities.as_ref());
    let status = match snapshot {
        None if checking => "Contacting host…".to_owned(),
        None => String::new(),
        Some(s) if !s.online => s.error.clone().unwrap_or_else(|| "Offline".into()),
        Some(s) => {
            let busy = s.info.as_ref().is_some_and(|i| i.is_busy());
            if !s.paired && record.is_paired() {
                "The host no longer recognises this device. Tap to pair again.".into()
            } else if !s.paired {
                "Tap to pair".into()
            } else if busy {
                "An app is running".into()
            } else {
                "Ready".into()
            }
        }
    };
    HostCard {
        id: record.id.as_str().into(),
        name: if record.name.is_empty() {
            record.address.as_str().into()
        } else {
            record.name.as_str().into()
        },
        address: format!("{}:{}", record.address, record.http_port).into(),
        online,
        checking: checking && snapshot.is_none(),
        paired,
        hermes: caps.is_some(),
        version: caps.map(|c| c.server_version.as_str()).unwrap_or_default().into(),
        status: status.into(),
    }
}

/// Short capability labels shown under the host name on the apps page.
pub fn host_chips(snapshot: &HostSnapshot) -> Vec<SharedString> {
    let mut chips: Vec<String> = Vec::new();
    match &snapshot.capabilities {
        Some(caps) => {
            let f = &caps.features;
            chips.push(if caps.server_version.is_empty() {
                "Hermes".into()
            } else {
                format!("Hermes {}", caps.server_version)
            });
            if f.virtual_display {
                let backends = f.virtual_display_backend.join("/").replace("hermes_kms", "Hermes-KMS");
                chips.push(if backends.is_empty() {
                    "Virtual display".into()
                } else {
                    format!("Virtual display · {backends}")
                });
            }
            if f.hdr_mode_control {
                chips.push("HDR".into());
            }
            if f.gamescope_session {
                chips.push("Gamescope".into());
            }
            if f.clipboard_sync {
                chips.push("Clipboard".into());
            }
            if caps.limits.max_width > 0 {
                chips.push(format!(
                    "Up to {}×{} @ {}",
                    caps.limits.max_width, caps.limits.max_height, caps.limits.max_fps
                ));
            }
        }
        None => chips.push("GameStream host".into()),
    }
    if let Some(info) = &snapshot.info {
        chips.extend(codecs::describe(info.codec_mode_support).into_iter().map(str::to_owned));
    }
    chips.into_iter().map(SharedString::from).collect()
}

pub fn app_card(app: &AppInfo, running_app: u32) -> AppCard {
    AppCard {
        id: app.id as i32,
        title: app.title.as_str().into(),
        art: Image::default(),
        has_art: false,
        running: running_app != 0 && running_app == app.id,
        hdr: app.hdr_supported,
    }
}

/// Decodes box art off the UI thread into raw RGBA (Slint images are not `Send`).
pub fn decode_art(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.thumbnail(360, 480).to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

pub fn art_image(raw: &[u8], width: u32, height: u32) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(raw, width, height))
}

pub fn load_settings_into(ui: &AppWindow, s: &StreamSettings) {
    let res = RESOLUTIONS
        .iter()
        .position(|&(w, h)| w == s.width && h == s.height)
        .unwrap_or(1);
    let fps = FRAME_RATES.iter().position(|&f| f == s.fps).unwrap_or(1);
    let codec = CodecPreference::ALL.iter().position(|&c| c == s.codec).unwrap_or(0);
    let audio = AUDIO_LAYOUTS.iter().position(|&a| a == s.audio).unwrap_or(0);
    ui.set_cfg_resolution(res as i32);
    ui.set_cfg_fps(fps as i32);
    ui.set_cfg_bitrate(s.bitrate_kbps as f32 / 1000.0);
    ui.set_cfg_codec(codec as i32);
    ui.set_cfg_audio(audio as i32);
    ui.set_cfg_hdr(s.hdr);
    ui.set_cfg_virtual_display(s.virtual_display);
    ui.set_cfg_host_audio(s.play_audio_on_host);
    ui.set_cfg_gamescope(s.launch_mode == LaunchMode::Gamescope);
    ui.set_cfg_optimize(s.optimize_game_settings);
    ui.set_cfg_clipboard(s.clipboard_sync);
}

pub fn read_settings_from(ui: &AppWindow, base: &StreamSettings) -> StreamSettings {
    let pick = |i: i32, len: usize| (i.max(0) as usize).min(len - 1);
    let (width, height) = RESOLUTIONS[pick(ui.get_cfg_resolution(), RESOLUTIONS.len())];
    StreamSettings {
        width,
        height,
        fps: FRAME_RATES[pick(ui.get_cfg_fps(), FRAME_RATES.len())],
        bitrate_kbps: (ui.get_cfg_bitrate().round() as u32).clamp(1, 500) * 1000,
        codec: CodecPreference::ALL[pick(ui.get_cfg_codec(), CodecPreference::ALL.len())],
        audio: AUDIO_LAYOUTS[pick(ui.get_cfg_audio(), AUDIO_LAYOUTS.len())],
        hdr: ui.get_cfg_hdr(),
        virtual_display: ui.get_cfg_virtual_display(),
        play_audio_on_host: ui.get_cfg_host_audio(),
        launch_mode: if ui.get_cfg_gamescope() {
            LaunchMode::Gamescope
        } else {
            LaunchMode::Normal
        },
        optimize_game_settings: ui.get_cfg_optimize(),
        clipboard_sync: ui.get_cfg_clipboard(),
        ..base.clone()
    }
}
