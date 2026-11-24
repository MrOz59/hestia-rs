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

/// Before a stream: Bluetooth on with the Wi-Fi on 2.4 GHz, the band
/// Bluetooth uses. The two take turns on the air, and the stream stutters
/// (worst with Bluetooth headphones playing). The warning's text, or `None`
/// when there's nothing to warn about or the platform can't tell.
pub fn shared_band_warning() -> Option<String> {
    shared_band(&radio_summary())
}

/// A value from [`radio_summary`]'s pairs.
pub fn radio_value<'a>(radio: &'a [(String, String)], label: &str) -> Option<&'a str> {
    radio.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str())
}

/// The band part of the Wi-Fi line ("2.4 GHz", "5 GHz", "off"...).
pub fn wifi_band(wifi: &str) -> &str {
    wifi.split(" (").next().unwrap_or(wifi)
}

fn shared_band(radio: &[(String, String)]) -> Option<String> {
    if radio_value(radio, "Bluetooth")? != "on" || wifi_band(radio_value(radio, "Wi-Fi")?) != "2.4 GHz" {
        return None;
    }
    let headphones = radio_value(radio, "Bluetooth audio outputs").is_some_and(|o| o != "none");
    let first = if headphones {
        "Bluetooth audio is connected and this Wi-Fi network is on 2.4 GHz, the band Bluetooth uses. \
         The two take turns on the air: expect the picture to stutter and the sound to drop out."
    } else {
        "Bluetooth is on and this Wi-Fi network is on 2.4 GHz, the band Bluetooth uses. Expect the \
         picture to stutter and the sound to drop out, worst with Bluetooth headphones."
    };
    Some(format!(
        "{first}\n\nFor a smooth stream, connect to the router's 5 GHz network and leave 2.4 GHz to \
         Bluetooth, or turn Bluetooth off."
    ))
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
    use super::{pairs, shared_band, wifi_band};

    fn radio(bluetooth: &str, outputs: &str, wifi: &str) -> Vec<(String, String)> {
        pairs(&format!(
            "Bluetooth\t{bluetooth}\nBluetooth audio outputs\t{outputs}\nWi-Fi\t{wifi}\n"
        ))
    }

    #[test]
    fn warns_only_for_bluetooth_on_with_wifi_on_2_4_ghz() {
        let wifi24 = "2.4 GHz (channel 9, 2452 MHz), -52 dBm, link 144/144 Mbps (send/receive)";
        let wifi5 = "5 GHz (channel 36, 5180 MHz), -60 dBm, link 866/866 Mbps (send/receive)";
        assert_eq!(wifi_band(wifi24), "2.4 GHz");
        assert_eq!(wifi_band("off"), "off");
        let on = shared_band(&radio("on", "none", wifi24)).expect("Bluetooth on, 2.4 GHz");
        assert!(on.contains("Bluetooth is on") && on.contains("5 GHz network"), "{on}");
        let playing = shared_band(&radio("on", "Bluetooth A2DP", wifi24)).expect("headphones");
        assert!(playing.contains("Bluetooth audio is connected"), "{playing}");
        assert_eq!(shared_band(&radio("off", "none", wifi24)), None);
        assert_eq!(shared_band(&radio("on", "Bluetooth A2DP", wifi5)), None);
        assert_eq!(shared_band(&radio("on", "none", "not connected")), None);
        assert_eq!(shared_band(&[]), None, "desktop: nothing to tell");
    }

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
