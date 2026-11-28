//! Offline Android input diagnostics. Uses a private instance of the same
//! mapper as streaming; opening the tester never allocates host gamepads.
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use hestia_core::stream::input::{GamepadState, buttons};

use crate::gamepad::{self, Axes, Control, Controllers};

const MAX_HISTORY: usize = 160;
const MAX_DEVICES: usize = 8;
const METRIC_WINDOW: usize = 512;
const NS_PER_MS: f64 = 1_000_000.0;
const AXIS_NAMES: [&str; 12] = [
    "X", "Y", "Z", "RZ", "RX", "RY", "LTRIGGER", "RTRIGGER", "BRAKE", "GAS", "HAT_X", "HAT_Y",
];

use super::controller_device::Device;
#[cfg(test)]
use super::controller_device::SOURCE_KEYBOARD;
#[cfg(target_os = "android")]
use super::controller_device::profiles;
use super::controller_device::{SOURCE_MOUSE, SOURCE_TOUCHPAD};

// Null entries are known non-controller devices and need no further JNI lookup.
type Inventory = BTreeMap<i32, Option<Device>>;

fn axis_values(a: Axes) -> [f32; 12] {
    [
        a.x, a.y, a.z, a.rz, a.rx, a.ry, a.ltrigger, a.rtrigger, a.brake, a.gas, a.hat_x, a.hat_y,
    ]
}

#[derive(Clone, Default)]
struct Window {
    values: VecDeque<u64>,
}

impl Window {
    fn push(&mut self, value: u64) {
        if self.values.len() == METRIC_WINDOW {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    fn summary(&self) -> String {
        if self.values.is_empty() {
            return "n/a".into();
        }
        let mut values: Vec<_> = self.values.iter().copied().collect();
        values.sort_unstable();
        let mean = values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64;
        let p95 = values[(values.len() * 95).div_ceil(100) - 1];
        let max = values[values.len() - 1];
        format!(
            "mean {:.3} ms | P95 {:.3} ms | max {:.3} ms (n={})",
            mean / NS_PER_MS,
            p95 as f64 / NS_PER_MS,
            max as f64 / NS_PER_MS,
            values.len()
        )
    }

    fn rate(&self) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        let mean = self.values.iter().map(|&v| v as f64).sum::<f64>() / self.values.len() as f64;
        (mean > 0.0).then_some(1_000_000_000.0 / mean)
    }
}

#[derive(Clone, Default)]
struct Timing {
    age: Window,
    intervals: Window,
    processing: Window,
    last_event: Option<i64>,
    historical: u64,
    invalid_times: u64,
    duplicates_or_reordered: u64,
}

impl Timing {
    fn record(&mut self, event_ns: i64, arrival_ns: i64, historical: bool, interval: bool) {
        self.historical += u64::from(historical);
        if event_ns <= 0 || arrival_ns < event_ns {
            self.invalid_times += 1;
            return;
        }
        self.age.push((arrival_ns - event_ns) as u64);
        if interval {
            if let Some(last) = self.last_event {
                if event_ns <= last {
                    self.duplicates_or_reordered += 1;
                    return;
                }
                // Long gaps are usually idle time, not a controller report interval.
                let delta = (event_ns - last) as u64;
                if delta <= 250_000_000 {
                    self.intervals.push(delta);
                }
            }
            self.last_event = Some(event_ns);
        }
    }

    fn report(&self, out: &mut String, label: &str, intervals: bool) {
        let _ = writeln!(out, "  {label} Android event age: {}", self.age.summary());
        let _ = writeln!(out, "  {label} diagnostic processing: {}", self.processing.summary());
        if intervals {
            let rate = self
                .intervals
                .rate()
                .map_or_else(|| "n/a".into(), |hz| format!("{hz:.1} Hz"));
            let _ = writeln!(out, "  {label} observed motion rate: {rate}");
            let _ = writeln!(out, "  {label} intervals: {}", self.intervals.summary());
            let _ = writeln!(
                out,
                "  {label} historical: {} | equal/out-of-order timestamps: {}",
                self.historical, self.duplicates_or_reordered
            );
        }
        if self.invalid_times > 0 {
            let _ = writeln!(out, "  {label} invalid timestamps: {}", self.invalid_times);
        }
    }
}

#[derive(Clone, Default)]
struct Touchpad {
    events: u64,
    action: u32,
    buttons: u32,
    seen_buttons: u32,
    x: f32,
    y: f32,
}

#[derive(Clone, Default)]
struct Sample {
    label: String,
    touchpad: Touchpad,
    mapped: GamepadState,
    axes: Option<([f32; 12], [f32; 12], [f32; 12])>,
    motion_count: u64,
    last_motion_source: u32,
    last_motion_action: u32,
    timing: [Timing; 4],
}

#[derive(Default)]
struct Tester {
    mapper: Controllers,
    samples: BTreeMap<i32, Sample>,
    keys: VecDeque<String>,
}

fn button_names(mask: u32) -> String {
    let names = [
        (buttons::A, "A"),
        (buttons::B, "B"),
        (buttons::X, "X"),
        (buttons::Y, "Y"),
        (buttons::LB, "LB"),
        (buttons::RB, "RB"),
        (buttons::BACK, "Back"),
        (buttons::PLAY, "Start"),
        (buttons::SPECIAL, "Guide"),
        (buttons::LS_CLICK, "LS"),
        (buttons::RS_CLICK, "RS"),
        (buttons::UP, "Up"),
        (buttons::DOWN, "Down"),
        (buttons::LEFT, "Left"),
        (buttons::RIGHT, "Right"),
    ];
    let active: Vec<_> = names
        .into_iter()
        .filter_map(|(bit, name)| (mask & bit != 0).then_some(name))
        .collect();
    if active.is_empty() {
        "none".into()
    } else {
        active.join(" + ")
    }
}

impl Tester {
    fn accepts(&mut self, devices: &Inventory, id: i32, event_matches: impl FnOnce(&Device) -> bool) -> bool {
        let Some(Some(device)) = devices.get(&id) else {
            return false;
        };
        if !device.is_controller() || !event_matches(device) {
            return false;
        }
        let Some(sample) = self.remember(id, None) else {
            return false;
        };
        if sample.label.is_empty() {
            sample.label.clone_from(&device.label);
        }
        true
    }

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
        let source_note = if gamepad::is_controller_source(source) {
            ""
        } else {
            " [source rejected by stream]"
        };
        self.keys.push_back(format!(
            "id {id} {action} scan {scan} key {code} src 0x{source:08x} => {mapped}{source_note}"
        ));
    }

    fn motion(&mut self, id: i32, source: u32, action: u32, axes: Axes) {
        let mapped = if source & gamepad::SOURCE_JOYSTICK == gamepad::SOURCE_JOYSTICK && action == 2 {
            self.mapper.motion(id, axes)
        } else {
            None
        };
        let Some(sample) = self.remember(id, mapped) else {
            return;
        };
        sample.motion_count += 1;
        sample.last_motion_source = source;
        sample.last_motion_action = action;
        let values = axis_values(axes);
        let (current, min, max) = sample.axes.get_or_insert((values, values, values));
        *current = values;
        for i in 0..values.len() {
            min[i] = min[i].min(values[i]);
            max[i] = max[i].max(values[i]);
        }
    }

    fn pointer(&mut self, id: i32, action: u32, buttons: u32, x: f32, y: f32) {
        if let Some(sample) = self.remember(id, None) {
            let p = &mut sample.touchpad;
            p.events += 1;
            p.action = action;
            p.buttons = buttons;
            p.seen_buttons |= buttons;
            p.x = x;
            p.y = y;
        }
    }

    // The sample source is independent from the GameStream preview. Historical
    // data contributes to raw extrema and timing, not the current mapper state.
    fn historical_motion(&mut self, id: i32, axes: Axes) {
        let Some(sample) = self.remember(id, None) else {
            return;
        };
        let values = axis_values(axes);
        let (_, min, max) = sample.axes.get_or_insert((values, values, values));
        for i in 0..values.len() {
            min[i] = min[i].min(values[i]);
            max[i] = max[i].max(values[i]);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn timing(&mut self, id: i32, lane: usize, event_ns: i64, arrival_ns: i64, historical: bool, processing_ns: u64) {
        if let Some(sample) = self.remember(id, None) {
            let timing = &mut sample.timing[lane];
            timing.record(event_ns, arrival_ns, historical, lane % 2 == 1);
            timing.processing.push(processing_ns);
        }
    }

    fn report(&self, devices: &Inventory) -> String {
        let mut out = String::from("Hestia controller tester — current mapping\n\n");
        let connected: Vec<_> = devices
            .iter()
            .filter_map(|(id, profile)| profile.as_ref().filter(|d| d.is_controller()).map(|d| (id, d)))
            .collect();
        if connected.is_empty() {
            out.push_str("No physical game controllers detected. Connect a gamepad to begin.\n");
        }
        for (id, d) in &connected {
            let _ = writeln!(out, "Connected ID {id}: {}", d.label);
        }
        if let Some(key) = self.keys.back() {
            let _ = writeln!(out, "\nLast key: {key}");
        }
        if self.samples.is_empty() && !connected.is_empty() {
            out.push_str("No controller events captured yet. Press a button or move a stick.\n");
        }
        for (id, sample) in &self.samples {
            let status = if connected.iter().any(|(current, _)| *current == id) {
                ""
            } else {
                " (disconnected)"
            };
            let _ = writeln!(out, "\nDevice {id}{status}: {}", sample.label);
            let p = &sample.touchpad;
            if p.events > 0 {
                let _ = writeln!(
                    out,
                    "  Touchpad pointer: {} events | x {:.1} y {:.1} | action {} | buttons 0x{:x} (seen 0x{:x})",
                    p.events, p.x, p.y, p.action, p.buttons, p.seen_buttons
                );
            }
            let s = sample.mapped;
            let _ = writeln!(
                out,
                "  Gamepad buttons {} (0x{:08x})",
                button_names(s.buttons),
                s.buttons
            );
            let _ = writeln!(
                out,
                "  LS {:?}  RS {:?}  LT {}  RT {}",
                s.left_stick, s.right_stick, s.left_trigger, s.right_trigger
            );
            for (lane, label) in ["Activity key", "Activity motion", "Native key", "Native motion"]
                .into_iter()
                .enumerate()
            {
                let timing = &sample.timing[lane];
                if !timing.age.values.is_empty() || timing.invalid_times > 0 {
                    timing.report(&mut out, label, lane % 2 == 1);
                }
            }
            if let Some((current, min, max)) = &sample.axes {
                let _ = writeln!(
                    out,
                    "  Motion events: {} | source 0x{:08x} | action {}",
                    sample.motion_count, sample.last_motion_source, sample.last_motion_action
                );
                out.push_str("  Raw axes: current [observed min, max]\n");
                for i in 0..AXIS_NAMES.len() {
                    let _ = writeln!(
                        out,
                        "  {} {:.3} [{:.3}, {:.3}]",
                        AXIS_NAMES[i], current[i], min[i], max[i]
                    );
                }
            }
        }
        out.push_str("\nRecent key events (newest first)\n");
        for event in self.keys.iter().rev() {
            let _ = writeln!(out, "{event}");
        }
        for (id, d) in connected {
            let _ = writeln!(out, "\nID {id}: {}", d.label);
            out.push_str(&d.details);
        }
        out
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use android_activity::input::{InputEvent, KeyAction, MotionAction};
    use jni::objects::JObject;
    use jni::sys::{jboolean, jfloat, jint, jlong};
    use jni::{JNIEnv, NativeMethod};

    use super::*;

    #[derive(Default)]
    struct Counts {
        keys: u64,
        motions: u64,
        pointers: u64,
        historical: u64,
    }

    #[derive(Default)]
    struct Session {
        tester: Tester,
        devices: Inventory,
        inventory_error: Option<String>,
        last_inventory: Option<Instant>,
        activity_status: String,
        dispatch_status: String,
        activity: Counts,
        native: Counts,
    }

    static SESSION: Mutex<Option<Session>> = Mutex::new(None);

    fn session() -> std::sync::MutexGuard<'static, Option<Session>> {
        SESSION.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn controller_tester_active(active: bool) {
        if active {
            *session() = Some(Session::default());
            let status = match set_activity_capture(true) {
                Ok(()) => "enabled".to_owned(),
                Err(error) => format!("unavailable ({error}); native fallback only"),
            };
            if let Some(s) = session().as_mut() {
                s.activity_status = status;
            }
        } else {
            let was_active = session().take().is_some();
            if was_active {
                let _ = set_activity_capture(false);
            }
        }
    }

    fn set_activity_capture(active: bool) -> Result<(), String> {
        use jni::objects::JValue;
        super::super::android::with_activity(|env, activity| {
            if active {
                let class = env.get_object_class(activity)?;
                let methods = [
                    NativeMethod {
                        name: "nativeControllerTesterKey".into(),
                        sig: "(IIIIIZJJ)Z".into(),
                        fn_ptr: activity_key as *mut std::ffi::c_void,
                    },
                    NativeMethod {
                        name: "nativeControllerTesterMotion".into(),
                        sig: "(IIIZJJFFFFFFFFFFFF)Z".into(),
                        fn_ptr: activity_motion as *mut std::ffi::c_void,
                    },
                    NativeMethod {
                        name: "nativeControllerTesterPointer".into(),
                        sig: "(IIIIFF)Z".into(),
                        fn_ptr: activity_pointer as *mut std::ffi::c_void,
                    },
                ];
                env.register_native_methods(&class, &methods)?;
                env.delete_local_ref(class)?;
            }
            env.call_method(
                activity,
                "setControllerTesterEnabled",
                "(Z)V",
                &[JValue::Bool(u8::from(active))],
            )?;
            Ok(())
        })
    }

    fn monotonic_ns() -> i64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime writes to a valid timespec; CLOCK_MONOTONIC
        // is the uptime time base used by Android input timestamps.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return 0;
        }
        ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
    }

    fn elapsed_ns(start: Instant) -> u64 {
        start.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }

    fn dispatch_status() -> String {
        use jni::objects::JString;
        let mut status = String::new();
        let result = super::super::android::with_activity(|env, activity| {
            let value = env
                .call_method(activity, "controllerTesterDispatchStatus", "()Ljava/lang/String;", &[])?
                .l()?;
            let value = JString::from(value);
            status = env.get_string(&value)?.into();
            env.delete_local_ref(value)?;
            Ok(())
        });
        if result.is_err() { "unavailable".into() } else { status }
    }

    extern "system" fn activity_pointer(
        _env: JNIEnv<'_>,
        _activity: JObject<'_>,
        id: jint,
        source: jint,
        action: jint,
        buttons: jint,
        x: jfloat,
        y: jfloat,
    ) -> jboolean {
        u8::from(controller_event(
            id,
            |d| d.accepts_pointer(source as u32),
            |s| {
                s.activity.pointers += 1;
                s.tester.pointer(id, action as u32, buttons as u32, x, y);
            },
        ))
    }

    extern "system" fn activity_key(
        _env: JNIEnv<'_>,
        _activity: JObject<'_>,
        id: jint,
        source: jint,
        scan: jint,
        code: jint,
        action: jint,
        repeat: jboolean,
        event_ms: jlong,
        arrival_ms: jlong,
    ) -> jboolean {
        let start = Instant::now();
        if !matches!(action, 0 | 1) {
            return 0;
        }
        u8::from(controller_event(
            id,
            |d| d.accepts_key(source as u32, code as u32, scan),
            |s| {
                s.activity.keys += 1;
                s.tester
                    .key(id, source as u32, scan, code as u32, action == 0, repeat != 0);
                s.tester.timing(
                    id,
                    0,
                    event_ms * 1_000_000,
                    arrival_ms * 1_000_000,
                    false,
                    elapsed_ns(start),
                );
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    extern "system" fn activity_motion(
        _env: JNIEnv<'_>,
        _activity: JObject<'_>,
        id: jint,
        source: jint,
        action: jint,
        historical: jboolean,
        event_ms: jlong,
        arrival_ms: jlong,
        x: jfloat,
        y: jfloat,
        z: jfloat,
        rz: jfloat,
        rx: jfloat,
        ry: jfloat,
        ltrigger: jfloat,
        rtrigger: jfloat,
        brake: jfloat,
        gas: jfloat,
        hat_x: jfloat,
        hat_y: jfloat,
    ) -> jboolean {
        let start = Instant::now();
        if action != 2 || source as u32 & gamepad::SOURCE_JOYSTICK != gamepad::SOURCE_JOYSTICK {
            return 0;
        }
        u8::from(controller_event(
            id,
            |d| d.joystick_axes,
            |s| {
                let axes = Axes {
                    x,
                    y,
                    z,
                    rz,
                    rx,
                    ry,
                    ltrigger,
                    rtrigger,
                    brake,
                    gas,
                    hat_x,
                    hat_y,
                };
                if historical != 0 {
                    s.activity.historical += 1;
                    s.tester.historical_motion(id, axes);
                } else {
                    s.activity.motions += 1;
                    s.tester.motion(id, source as u32, action as u32, axes);
                }
                s.tester.timing(
                    id,
                    1,
                    event_ms * 1_000_000,
                    arrival_ms * 1_000_000,
                    historical != 0,
                    elapsed_ns(start),
                );
            },
        ))
    }

    // A newly connected controller can deliver its first button before the
    // next inventory refresh. Resolve it once, outside the session lock.
    fn controller_event(id: i32, accepts: impl FnOnce(&Device) -> bool, record: impl FnOnce(&mut Session)) -> bool {
        if id < 0 || super::super::gamepads::streaming() {
            return false;
        }
        let mut guard = session();
        let Some(s) = guard.as_ref() else { return false };
        if !s.devices.contains_key(&id) {
            drop(guard);
            let Ok(profile) = profiles(Some(id)) else { return false };
            guard = session();
            let Some(s) = guard.as_mut() else { return false };
            s.devices.entry(id).or_insert(profile);
        }
        let Some(s) = guard.as_mut() else { return false };
        if !s.tester.accepts(&s.devices, id, accepts) {
            return false;
        }
        record(s);
        true
    }

    pub fn controller_tester_report() -> String {
        let refresh = session().as_ref().is_some_and(|s| {
            s.last_inventory
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        });
        // Do not hold the state lock across JNI.
        let devices = refresh.then(|| profiles(None));
        let dispatch = refresh.then(dispatch_status);
        let mut guard = session();
        let Some(s) = guard.as_mut() else { return String::new() };
        if let Some(devices) = devices {
            match devices {
                Ok(devices) => {
                    s.devices = devices;
                    s.inventory_error = None;
                }
                Err(error) => s.inventory_error = Some(error),
            }
            s.last_inventory = Some(Instant::now());
        }
        if let Some(dispatch) = dispatch {
            s.dispatch_status = dispatch;
        }
        let mut out = s.tester.report(&s.devices);
        let _ = writeln!(
            out,
            "\nCapture: Android Activity {}\nUnbuffered dispatch: {}",
            s.activity_status, s.dispatch_status
        );
        out.push_str(concat!(
            "Timing: Android event to callback (not physical Bluetooth latency). ",
            "Activity timestamps: milliseconds; native: nanoseconds. ",
            "Motion rate uses recent non-idle timestamp intervals, not HID polling rate.\n"
        ));
        for (name, c) in [("Activity", &s.activity), ("Native queue", &s.native)] {
            let _ = writeln!(
                out,
                "{name} captured: {} keys, {} joystick motions, {} historical samples, {} touchpad events",
                c.keys, c.motions, c.historical, c.pointers
            );
        }
        if let Some(error) = &s.inventory_error {
            let _ = writeln!(out, "Could not refresh controller inventory: {error}");
        }
        out
    }

    pub(super) fn capture(event: &InputEvent<'_>) -> bool {
        // The tester is deliberately offline and has no stream sink.
        if session().is_none() || super::super::gamepads::streaming() {
            return false;
        }
        match event {
            InputEvent::KeyEvent(key) => {
                let start = Instant::now();
                let arrival = monotonic_ns();
                let source = u32::from(key.source());
                let code = u32::from(key.key_code());
                let id = key.device_id();
                let pressed = match key.action() {
                    KeyAction::Down => true,
                    KeyAction::Up => false,
                    _ => return false,
                };
                controller_event(
                    id,
                    |d| d.accepts_key(source, code, key.scan_code()),
                    |s| {
                        s.native.keys += 1;
                        s.tester
                            .key(id, source, key.scan_code(), code, pressed, key.repeat_count() > 0);
                        s.tester
                            .timing(id, 2, key.event_time(), arrival, false, elapsed_ns(start));
                    },
                )
            }
            InputEvent::MotionEvent(motion) => {
                let start = Instant::now();
                let arrival = monotonic_ns();
                let source = u32::from(motion.source());
                let id = motion.device_id();
                if motion.pointer_count() == 0 {
                    return false;
                }
                if source & SOURCE_MOUSE == SOURCE_MOUSE || source & SOURCE_TOUCHPAD == SOURCE_TOUCHPAD {
                    return controller_event(
                        id,
                        |d| d.accepts_pointer(source),
                        |s| {
                            let pointer = motion.pointer_at_index(0);
                            s.native.pointers += 1;
                            s.tester.pointer(
                                id,
                                motion.action().into(),
                                motion.button_state().0,
                                pointer.x(),
                                pointer.y(),
                            );
                        },
                    );
                }
                if source & gamepad::SOURCE_JOYSTICK != gamepad::SOURCE_JOYSTICK
                    || motion.action() != MotionAction::Move
                {
                    return false;
                }
                controller_event(
                    id,
                    |d| d.joystick_axes,
                    |s| {
                        s.native.motions += 1;
                        let pointer = motion.pointer_at_index(0);
                        for historical in pointer.history() {
                            use android_activity::input::Axis;
                            let start = Instant::now();
                            let axes = Axes {
                                x: historical.axis_value(Axis::X),
                                y: historical.axis_value(Axis::Y),
                                z: historical.axis_value(Axis::Z),
                                rz: historical.axis_value(Axis::Rz),
                                rx: historical.axis_value(Axis::Rx),
                                ry: historical.axis_value(Axis::Ry),
                                ltrigger: historical.axis_value(Axis::Ltrigger),
                                rtrigger: historical.axis_value(Axis::Rtrigger),
                                brake: historical.axis_value(Axis::Brake),
                                gas: historical.axis_value(Axis::Gas),
                                hat_x: historical.axis_value(Axis::HatX),
                                hat_y: historical.axis_value(Axis::HatY),
                            };
                            s.native.historical += 1;
                            s.tester.historical_motion(id, axes);
                            s.tester
                                .timing(id, 3, historical.event_time(), arrival, true, elapsed_ns(start));
                        }
                        s.tester.motion(
                            motion.device_id(),
                            source,
                            2,
                            super::super::gamepads::android_axes(motion),
                        );
                        s.tester
                            .timing(id, 3, motion.event_time(), arrival, false, elapsed_ns(start));
                    },
                )
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

    fn profile(name: &str, sources: u32, joystick_axes: bool, pad_buttons: u32, alphabetic: bool) -> Option<Device> {
        Some(Device {
            vendor: 0,
            product: 0,
            keys: vec![],
            hat_x: false,
            hat_y: false,
            analog_triggers: false,
            rumble: false,
            label: name.into(),
            details: String::new(),
            sources,
            joystick_axes,
            pad_buttons,
            alphabetic,
        })
    }

    #[test]
    fn only_verified_controllers_enter_inventory_and_capture() {
        // IDs and capabilities of the two identifiable entries in the Xiaomi report.
        let mut devices = Inventory::from([
            (3, profile("uinput-xiaomi", 0x501, false, 1, false)),
            (
                13,
                profile(
                    "Wireless Controller Touchpad | Sony DualShock 4",
                    0x0510251b,
                    true,
                    13,
                    false,
                ),
            ),
            (14, profile("Physical keyboard", 0x501, false, 3, true)),
            (15, profile("TV remote", gamepad::SOURCE_DPAD, false, 0, false)),
            (16, profile("Phone touchscreen", 0x1002, false, 0, false)),
            (-1, None), // Android virtual keyboard
        ]);
        let mut tester = Tester::default();
        for id in [-1, 3, 12, 14, 15, 16] {
            // Even a convincing button code, scan or joystick event cannot create a controller.
            assert!(!tester.accepts(&devices, id, |d| d.accepts_key(0x501, 96, 304)));
            assert!(!tester.accepts(&devices, id, |d| d.joystick_axes));
        }
        assert!(tester.samples.is_empty());
        assert!(tester.accepts(&devices, 13, |d| d.accepts_key(0x501, 104, 312)));
        assert!(tester.accepts(&devices, 13, |d| d.accepts_key(SOURCE_KEYBOARD, 0, 306)));
        assert!(tester.accepts(&devices, 13, |d| d.accepts_key(SOURCE_KEYBOARD, 0, 704)));
        assert!(tester.accepts(&devices, 13, |d| d.accepts_key(gamepad::SOURCE_DPAD, 19, 0)));
        assert!(!tester.accepts(&devices, 13, |d| d.accepts_key(SOURCE_KEYBOARD, 29, 30)));
        tester.key(13, 0x501, 312, 104, true, false);
        let report = tester.report(&devices);
        for ignored in ["uinput-xiaomi", "Physical keyboard", "TV remote", "Phone touchscreen"] {
            assert!(!report.contains(ignored));
        }
        assert!(report.contains("id 13 DOWN scan 312 key 104"));
        assert!(devices.get(&3).unwrap().as_ref().unwrap().gamepad_info().is_none());
        let ds4 = devices.get_mut(&13).unwrap().as_mut().unwrap();
        ds4.vendor = 0x054c;
        ds4.product = 0x09cc;
        ds4.keys = vec![96, 100, 108];
        ds4.hat_x = true;
        ds4.analog_triggers = true;
        ds4.rumble = true;
        let info = ds4.gamepad_info().unwrap();
        assert_eq!(
            info.model,
            Some(hestia_core::stream::input::ControllerModel::DualShock4)
        );
        assert_eq!(info.capabilities, 3);
        assert_eq!(
            info.supported_buttons,
            buttons::A | buttons::Y | buttons::PLAY | buttons::LEFT | buttons::RIGHT
        );
        devices.remove(&13);
        let report = tester.report(&devices);
        assert!(report.contains("No physical game controllers detected"));
        assert!(report.contains("Device 13 (disconnected): Wireless Controller Touchpad | Sony DualShock 4"));
    }

    #[test]
    fn accepts_digital_pads_and_only_the_verified_controllers_touchpad() {
        let devices = Inventory::from([
            (1, profile("Digital pad", gamepad::SOURCE_GAMEPAD, false, 2, false)),
            (2, profile("Mouse", SOURCE_MOUSE, false, 0, false)),
            (
                3,
                profile("Laptop touchpad", SOURCE_MOUSE | SOURCE_TOUCHPAD, false, 0, false),
            ),
            (13, profile("DS4", 0x0510251b, true, 13, false)),
        ]);
        let mut tester = Tester::default();
        assert!(tester.accepts(&devices, 1, |d| d.accepts_key(gamepad::SOURCE_GAMEPAD, 96, 304)));
        for id in [1, 2, 3] {
            assert!(!tester.accepts(&devices, id, |d| d.accepts_pointer(SOURCE_MOUSE)));
        }
        assert!(!tester.accepts(&devices, 13, |d| d.accepts_pointer(0x1002)));
        assert!(tester.accepts(&devices, 13, |d| d.accepts_pointer(SOURCE_TOUCHPAD)));
        assert!(tester.accepts(&devices, 13, |d| d.accepts_pointer(SOURCE_MOUSE)));
        tester.pointer(13, 0, 1, 8.0, 10.0);
        tester.pointer(13, 1, 0, 16.0, 20.0);
        let sample = &tester.samples[&13];
        assert_eq!(sample.touchpad.events, 2);
        assert_eq!(sample.touchpad.seen_buttons, 1);
        assert!(sample.axes.is_none());
        assert_eq!(sample.motion_count, 0);
        assert_eq!(sample.mapped.buttons, 0);
        assert!(tester.keys.is_empty());
    }

    #[test]
    fn timing_uses_bounded_windows_and_excludes_idle_and_reordered_intervals() {
        let mut timing = Timing::default();
        timing.record(1_000_000, 3_000_000, true, true);
        timing.record(5_000_000, 6_000_000, false, true);
        timing.record(5_000_000, 7_000_000, false, true);
        timing.record(400_000_000, 401_000_000, false, true);
        timing.record(404_000_000, 405_000_000, false, true);
        timing.record(500_000_000, 499_000_000, false, true);
        assert_eq!(timing.intervals.rate(), Some(250.0));
        assert_eq!(timing.historical, 1);
        assert_eq!(timing.invalid_times, 1);
        assert_eq!(timing.duplicates_or_reordered, 1);
        for i in 0..METRIC_WINDOW + 10 {
            timing.processing.push(i as u64);
        }
        assert_eq!(timing.processing.values.len(), METRIC_WINDOW);
    }

    #[test]
    fn historical_axes_keep_extrema_without_changing_current_mapping() {
        let mut tester = Tester::default();
        tester.historical_motion(
            2,
            Axes {
                x: 1.0,
                ..Default::default()
            },
        );
        tester.motion(2, gamepad::SOURCE_JOYSTICK, 2, Axes::default());
        let report = tester.report(&Inventory::new());
        assert!(report.contains("X 0.000 [0.000, 1.000]"));
        assert_eq!(tester.samples[&2].mapped.left_stick, (0, 0));
    }

    #[test]
    fn reports_raw_and_current_mapping_even_when_wrong_or_unknown() {
        let mut tester = Tester::default();
        tester.key(7, gamepad::SOURCE_GAMEPAD, 308, 109, true, false);
        assert_eq!(tester.samples[&7].mapped.buttons, buttons::BACK);
        tester.key(7, gamepad::SOURCE_GAMEPAD, 308, 109, false, false);
        tester.key(7, gamepad::SOURCE_GAMEPAD, 306, 0, true, false);
        let report = tester.report(&Inventory::new());
        assert!(report.contains("scan 308 key 109"));
        assert!(report.contains("=> Back"));
        assert!(report.contains("scan 306 key 0"));
        assert!(report.contains("=> unmapped"));
        assert_eq!(tester.samples[&7].mapped.buttons, 0);
    }

    #[test]
    fn records_axis_extrema_after_release() {
        let mut tester = Tester::default();
        tester.motion(
            2,
            gamepad::SOURCE_JOYSTICK,
            2,
            Axes {
                rx: -1.0,
                ltrigger: 0.5,
                ..Default::default()
            },
        );
        tester.motion(2, gamepad::SOURCE_JOYSTICK, 2, Axes::default());
        let report = tester.report(&Inventory::new());
        assert!(report.contains("RX 0.000 [-1.000, 0.000]"));
        assert!(report.contains("LTRIGGER 0.000 [0.000, 0.500]"));
    }

    #[test]
    fn records_unmapped_motion_without_changing_stream_state() {
        let mut tester = Tester::default();
        tester.motion(
            2,
            gamepad::SOURCE_GAMEPAD,
            2,
            Axes {
                x: 1.0,
                ..Default::default()
            },
        );
        assert_eq!(tester.samples[&2].mapped.left_stick, (0, 0));
        let report = tester.report(&Inventory::new());
        assert!(report.contains("X 1.000 [1.000, 1.000]"));
        assert!(report.contains("Motion events: 1"));
    }

    #[test]
    fn reports_rejected_sources_without_changing_mapper_and_bounds_history() {
        let mut tester = Tester::default();
        tester.key(7, gamepad::SOURCE_DPAD, 304, 96, true, false);
        assert_eq!(tester.samples[&7].mapped.buttons, 0);
        assert!(tester.report(&Inventory::new()).contains("source rejected by stream"));
        for _ in 0..MAX_HISTORY + 10 {
            tester.key(7, gamepad::SOURCE_GAMEPAD, 304, 96, true, false);
        }
        assert_eq!(tester.keys.len(), MAX_HISTORY);
    }
}
