//! The same Android device and capability checks drive streaming and the offline tester.
use crate::gamepad;
use hestia_core::stream::input::{GamepadInfo, buttons};
use serde::Deserialize;

pub(super) const SOURCE_KEYBOARD: u32 = 0x0000_0101;
pub(super) const SOURCE_MOUSE: u32 = 0x0000_2002;
pub(super) const SOURCE_TOUCHPAD: u32 = 0x0010_0008;

#[derive(Deserialize)]
pub(super) struct Device {
    pub(super) label: String,
    pub(super) details: String,
    pub(super) sources: u32,
    pub(super) joystick_axes: bool,
    pub(super) pad_buttons: u32,
    pub(super) vendor: u16,
    pub(super) product: u16,
    pub(super) keys: Vec<u32>,
    pub(super) hat_x: bool,
    pub(super) hat_y: bool,
    pub(super) analog_triggers: bool,
    pub(super) rumble: bool,
    pub(super) alphabetic: bool,
}

// Android also sets GAMEPAD on keyboards, remotes and OEM input helpers with
// a single gamepad key. Digital pads may lack sticks and handhelds can be internal.
impl Device {
    pub(super) fn is_controller(&self) -> bool {
        (self.sources & gamepad::SOURCE_JOYSTICK == gamepad::SOURCE_JOYSTICK && self.joystick_axes)
            || (self.sources & gamepad::SOURCE_GAMEPAD == gamepad::SOURCE_GAMEPAD
                && !self.alphabetic
                && self.pad_buttons >= 2)
    }

    pub(super) fn gamepad_info(&self) -> Option<GamepadInfo> {
        if !self.is_controller() {
            return None;
        }
        let mut info = GamepadInfo::from_usb(self.vendor, self.product);
        info.capabilities = u16::from(self.analog_triggers) | (u16::from(self.rumble) << 1);
        for &key in &self.keys {
            if let Some(gamepad::Control::Button(bit)) = gamepad::control_for_keycode(key) {
                info.supported_buttons |= bit;
            }
        }
        if self.hat_x {
            info.supported_buttons |= buttons::LEFT | buttons::RIGHT;
        }
        if self.hat_y {
            info.supported_buttons |= buttons::UP | buttons::DOWN;
        }
        Some(info)
    }

    pub(super) fn accepts_key(&self, source: u32, code: u32, scan: i32) -> bool {
        if gamepad::is_controller_source(source) && !self.alphabetic {
            return true;
        }
        // Some controller buttons arrive as keyboard or d-pad events. Allow
        // those signatures only after validating the originating device.
        (gamepad::is_controller_source(source)
            || source == SOURCE_KEYBOARD
            || source & gamepad::SOURCE_DPAD == gamepad::SOURCE_DPAD)
            && (matches!(code, 19..=23 | 96..=110 | 188..=203)
                || (0x130..=0x13f).contains(&scan)
                || (!self.alphabetic && code == 0 && scan > 0))
    }

    pub(super) fn accepts_pointer(&self, source: u32) -> bool {
        self.sources & SOURCE_TOUCHPAD == SOURCE_TOUCHPAD
            && (source & SOURCE_MOUSE == SOURCE_MOUSE || source & SOURCE_TOUCHPAD == SOURCE_TOUCHPAD)
    }
}

#[cfg(target_os = "android")]
pub(super) fn profiles<T: serde::de::DeserializeOwned>(id: Option<i32>) -> Result<T, String> {
    use jni::objects::{JString, JValue};
    let mut json = String::new();
    super::android::with_helper(|env, _, helper| {
        let value = match id {
            Some(id) => env.call_static_method(
                helper,
                "controllerTesterDevice",
                "(I)Ljava/lang/String;",
                &[JValue::Int(id)],
            )?,
            None => env.call_static_method(helper, "controllerTesterDevices", "()Ljava/lang/String;", &[])?,
        }
        .l()?;
        let value = env.auto_local(JString::from(value));
        json = env.get_string(&value)?.into();
        Ok(())
    })?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}
