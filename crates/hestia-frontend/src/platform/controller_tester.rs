//! Offline Android input diagnostics. Uses a private instance of the same
//! mapper as streaming; opening the tester never allocates host gamepads.
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use hestia_core::stream::input::{buttons, GamepadState};
use crate::gamepad::{self, Axes, Control, Controllers};

const MAX_HISTORY: usize = 96;
const MAX_DEVICES: usize = 8;
const AXIS_NAMES: [&str; 12] = [
    "X", "Y", "Z", "RZ", "RX", "RY", "LTRIGGER", "RTRIGGER", "BRAKE", "GAS", "HAT_X", "HAT_Y",
];

fn axis_values(a: Axes) -> [f32; 12] {
    [a.x, a.y, a.z, a.rz, a.rx, a.ry, a.ltrigger, a.rtrigger, a.brake, a.gas, a.hat_x, a.hat_y]
}

#[derive(Default)]
struct Sample {
    mapped: GamepadState,
    axes: Option<([f32; 12], [f32; 12], [f32; 12])>,
}

#[derive(Default)]
struct Tester {
    mapper: Controllers,
    samples: BTreeMap<i32, Sample>,
    keys: VecDeque<String>,
}

fn button_names(mask: u32) -> String {
    let names = [
        (buttons::A, "A"), (buttons::B, "B"), (buttons::X, "X"), (buttons::Y, "Y"),
        (buttons::LB, "LB"), (buttons::RB, "RB"), (buttons::BACK, "Back"),
        (buttons::PLAY, "Start"), (buttons::SPECIAL, "Guide"),
        (buttons::LS_CLICK, "LS"), (buttons::RS_CLICK, "RS"),
        (buttons::UP, "Up"), (buttons::DOWN, "Down"),
        (buttons::LEFT, "Left"), (buttons::RIGHT, "Right"),
    ];
    let active: Vec<_> = names.into_iter().filter_map(|(bit, name)| (mask & bit != 0).then_some(name)).collect();
    if active.is_empty() { "none".into() } else { active.join(" + ") }
}

impl Tester {
    fn remember(&mut self, id: i32, state: Option<GamepadState>) -> Option<&mut Sample> {
        if !self.samples.contains_key(&id) && self.samples.len() >= MAX_DEVICES {
            return None;
        }
        let sample = self.samples.entry(id).or_default();
        if let Some(state) = state {
            sample.mapped = state;
        }
        Some(sample)
    }

    fn key(&mut self, id: i32, source: u32, scan: i32, code: u32, pressed: bool, repeat: bool) {
        let mapped = match gamepad::control_for_keycode(code) {
            Some(Control::Button(bit)) => button_names(bit),
            Some(Control::LeftTrigger) => "LT".into(),
            Some(Control::RightTrigger) => "RT".into(),
            None => "unmapped".into(),
        };
        // Keep the report faithful to streaming, including repeat filtering
        // and source filtering (the tester can observe more than it forwards).
        let state = if gamepad::is_controller_source(source) && !(pressed && repeat) {
            self.mapper.key(id, code, pressed)
        } else {
            None
        };
        self.remember(id, state);
        if pressed && repeat {
            return;
        }
        if self.keys.len() >= MAX_HISTORY {
            self.keys.pop_front();
        }
        let action = if pressed { "DOWN" } else { "UP" };
        let source_note = if gamepad::is_controller_source(source) { "" } else { " [source rejected by stream]" };
        self.keys.push_back(format!(
            "id {id} {action} scan {scan} key {code} src 0x{source:08x} => {mapped}{source_note}"
        ));
    }

    fn motion(&mut self, id: i32, axes: Axes) {
        let mapped = self.mapper.motion(id, axes);
        let Some(sample) = self.remember(id, mapped) else { return };
        let values = axis_values(axes);
        let (current, min, max) = sample.axes.get_or_insert((values, values, values));
        *current = values;
        for i in 0..values.len() {
            min[i] = min[i].min(values[i]);
            max[i] = max[i].max(values[i]);
        }
    }

    fn report(&self, devices: &str) -> String {
        let mut out = String::from("Hestia controller tester — current mapping (no corrections)\n\n");
        if self.samples.is_empty() {
            out.push_str("Waiting for controller input…\n");
        }
        for (id, sample) in &self.samples {
            let s = sample.mapped;
            let _ = writeln!(out, "Device {id}: buttons {} (0x{:08x})", button_names(s.buttons), s.buttons);
            let _ = writeln!(out, "  LS {:?}  RS {:?}  LT {}  RT {}", s.left_stick, s.right_stick, s.left_trigger, s.right_trigger);
            if let Some((current, min, max)) = &sample.axes {
                out.push_str("  Raw axes: current [observed min, max]\n");
                for i in 0..AXIS_NAMES.len() {
                    let _ = writeln!(out, "  {} {:.3} [{:.3}, {:.3}]", AXIS_NAMES[i], current[i], min[i], max[i]);
                }
            }
        }
        out.push_str("\nRecent key events (newest first)\n");
        for event in self.keys.iter().rev() {
            let _ = writeln!(out, "{event}");
        }
        out.push_str("\nAndroid input devices and advertised axis ranges\n");
        out.push_str(devices);
        out
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use android_activity::input::{InputEvent, KeyAction, MotionAction};
    use super::*;

    #[derive(Default)]
    struct Session {
        tester: Tester,
        devices: String,
        last_inventory: Option<Instant>,
    }

    static SESSION: Mutex<Option<Session>> = Mutex::new(None);

    fn session() -> std::sync::MutexGuard<'static, Option<Session>> {
        SESSION.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn controller_tester_active(active: bool) {
        *session() = active.then(Session::default);
    }

    fn inventory() -> String {
        use jni::objects::JString;
        let mut text = String::new();
        let result = super::super::android::with_helper(|env, _, helper| {
            let value = env.call_static_method(helper, "controllerTesterDevices", "()Ljava/lang/String;", &[])?.l()?;
            let value = JString::from(value);
            text = env.get_string(&value)?.into();
            env.delete_local_ref(value)?;
            Ok(())
        });
        if let Err(error) = result { format!("Could not enumerate input devices: {error}") } else { text }
    }

    fn is_controller(id: i32) -> bool {
        use jni::objects::JValue;
        let mut found = false;
        let _ = super::super::android::with_helper(|env, _, helper| {
            found = env.call_static_method(helper, "controllerTesterIsDevice", "(I)Z", &[JValue::Int(id)])?.z()?;
            Ok(())
        });
        found
    }

    pub fn controller_tester_report() -> String {
        let refresh = session().as_ref().is_some_and(|s| {
            s.last_inventory.is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        });
        // Do not hold the state lock across JNI.
        let devices = refresh.then(inventory);
        let mut guard = session();
        let Some(s) = guard.as_mut() else { return String::new() };
        if let Some(devices) = devices {
            s.devices = devices;
            s.last_inventory = Some(Instant::now());
        }
        s.tester.report(&s.devices)
    }

    pub(super) fn capture(event: &InputEvent<'_>) -> bool {
        // The tester is deliberately offline and has no stream sink.
        if session().is_none() || super::super::gamepads::streaming() {
            return false;
        }
        match event {
            InputEvent::KeyEvent(key) => {
                let source = u32::from(key.source());
                let code = u32::from(key.key_code());
                let id = key.device_id();
                if !gamepad::is_controller_source(source) && !is_controller(id) {
                    return false; // TV remote, keyboard and touch stay usable.
                }
                let pressed = match key.action() {
                    KeyAction::Down => true,
                    KeyAction::Up => false,
                    _ => return false,
                };
                let mut guard = session();
                let Some(s) = guard.as_mut() else { return false };
                s.tester.key(id, source, key.scan_code(), code, pressed, key.repeat_count() > 0);
                // Even unknown controller buttons must not navigate the settings.
                true
            }
            InputEvent::MotionEvent(motion) => {
                let source = u32::from(motion.source());
                if source & gamepad::SOURCE_JOYSTICK != gamepad::SOURCE_JOYSTICK || motion.action() != MotionAction::Move {
                    return false;
                }
                let mut guard = session();
                let Some(s) = guard.as_mut() else { return false };
                s.tester.motion(motion.device_id(), super::super::gamepads::android_axes(motion));
                true
            }
            _ => false,
        }
    }
}

#[cfg(target_os = "android")]
pub use android::{controller_tester_active, controller_tester_report};
#[cfg(target_os = "android")]
pub(super) fn capture(event: &android_activity::input::InputEvent<'_>) -> bool {
    android::capture(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_raw_and_current_mapping_even_when_wrong_or_unknown() {
        let mut tester = Tester::default();
        tester.key(7, gamepad::SOURCE_GAMEPAD, 308, 109, true, false);
        assert_eq!(tester.samples[&7].mapped.buttons, buttons::BACK);
        tester.key(7, gamepad::SOURCE_GAMEPAD, 308, 109, false, false);
        tester.key(7, gamepad::SOURCE_GAMEPAD, 306, 0, true, false);
        let report = tester.report("Sony DualShock 4");
        assert!(report.contains("scan 308 key 109"));
        assert!(report.contains("=> Back"));
        assert!(report.contains("scan 306 key 0"));
        assert!(report.contains("=> unmapped"));
        assert!(report.contains("Sony DualShock 4"));
        assert_eq!(tester.samples[&7].mapped.buttons, 0);
    }

    #[test]
    fn records_axis_extrema_after_release() {
        let mut tester = Tester::default();
        tester.motion(2, Axes { rx: -1.0, ltrigger: 0.5, ..Default::default() });
        tester.motion(2, Axes::default());
        let report = tester.report("");
        assert!(report.contains("RX 0.000 [-1.000, 0.000]"));
        assert!(report.contains("LTRIGGER 0.000 [0.000, 0.500]"));
    }

    #[test]
    fn reports_rejected_sources_without_changing_mapper_and_bounds_history() {
        let mut tester = Tester::default();
        tester.key(7, gamepad::SOURCE_DPAD, 304, 96, true, false);
        assert_eq!(tester.samples[&7].mapped.buttons, 0);
        assert!(tester.report("").contains("source rejected by stream"));
        for _ in 0..MAX_HISTORY + 10 {
            tester.key(7, gamepad::SOURCE_GAMEPAD, 304, 96, true, false);
        }
        assert_eq!(tester.keys.len(), MAX_HISTORY);
    }
}
