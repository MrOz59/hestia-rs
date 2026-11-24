//! The radios around a stream, for the session report: Bluetooth (on
//! 2.4 GHz it takes air time from Wi-Fi), the Wi-Fi link, and where the
//! audio plays.

#[cfg(target_os = "android")]
use super::android::with_helper;

/// Bluetooth and Wi-Fi state as `(label, value)` pairs: Bluetooth on or
/// off, the Bluetooth audio outputs connected, and the Wi-Fi band, signal
/// and rate. Empty where the platform can't tell (desktop).
pub fn radio_summary() -> Vec<(String, String)> {
    #[cfg(target_os = "android")]
    {
        let mut text = String::new();
        let result = with_helper(|env, activity, helper| {
            use jni::objects::{JString, JValue};
            let value = env
                .call_static_method(
                    helper,
                    "radioSummary",
                    "(Landroid/app/Activity;)Ljava/lang/String;",
                    &[JValue::Object(activity)],
                )?
                .l()?;
            if !value.is_null() {
                text = env.get_string(&JString::from(value))?.into();
            }
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("reading the Bluetooth and Wi-Fi state failed: {e}");
        }
        pairs(&text)
    }
    #[cfg(not(target_os = "android"))]
    Vec::new()
}

/// What the audio output with this id is ("Bluetooth A2DP", "built-in
/// speaker"); `None` where the platform has no output ids.
pub fn audio_output_name(device_id: i64) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        let mut name: Option<String> = None;
        let id = i32::try_from(device_id).unwrap_or(0);
        let result = with_helper(|env, activity, helper| {
            use jni::objects::{JString, JValue};
            let value = env
                .call_static_method(
                    helper,
                    "audioOutputName",
                    "(Landroid/app/Activity;I)Ljava/lang/String;",
                    &[JValue::Object(activity), JValue::Int(id)],
                )?
                .l()?;
            if !value.is_null() {
                name = Some(env.get_string(&JString::from(value))?.into());
            }
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("looking up audio output {device_id} failed: {e}");
        }
        name
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = device_id;
        None
    }
}

/// `label<TAB>value` lines, blank and malformed ones skipped.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, value)| (label.trim().to_owned(), value.trim().to_owned()))
        .filter(|(label, value)| !label.is_empty() && !value.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::pairs;

    #[test]
    fn helper_lines_become_report_fields() {
        let text =
            "Bluetooth\ton\nBluetooth audio outputs\tBluetooth A2DP\n\nbroken line\nWi-Fi\t2.4 GHz (channel 6)\n";
        assert_eq!(
            pairs(text),
            [
                ("Bluetooth".to_owned(), "on".to_owned()),
                ("Bluetooth audio outputs".to_owned(), "Bluetooth A2DP".to_owned()),
                ("Wi-Fi".to_owned(), "2.4 GHz (channel 6)".to_owned()),
            ]
        );
    }
}
