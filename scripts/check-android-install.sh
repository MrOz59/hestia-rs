#!/usr/bin/env bash
set -euo pipefail

candidate=$1
package=com.mroz59.hestia.rs
component="$package/android.app.NativeActivity"

check_launch() {
    local category resolved
    for category in LAUNCHER LEANBACK_LAUNCHER; do
        resolved=$(adb shell cmd package resolve-activity --brief \
            -a android.intent.action.MAIN -c "android.intent.category.$category" -p "$package")
        [[ "$resolved" == *"$component"* ]]
    done

    adb logcat -c
    local launch
    launch=$(adb shell am start -W -n "$component")
    printf '%s\n' "$launch"
    [[ "$launch" == *"Status: ok"* ]]
    # The message is emitted after Java launched the NativeActivity and
    # Rust registered its callbacks, so it also checks that DEX loads.
    local attempt logs
    for ((attempt = 0; attempt < 20; attempt++)); do
        logs=$(adb logcat -d -s hestia:I AndroidRuntime:E)
        if [[ "$logs" == *"Android picture-in-picture callbacks registered"* ]] \
            && adb shell pidof "$package" >/dev/null; then
            adb shell am force-stop "$package"
            return
        fi
        sleep 1
    done
    printf '%s\n' "$logs"
    echo "::error::Hestia RS did not start its picture-in-picture activity"
    return 1
}

echo 'Checking fresh install of the universal APK'
adb install "$candidate"
check_launch

if [[ -n "${PREVIOUS_NIGHTLY_APK:-}" ]]; then
    echo 'Checking update from the previously published universal APK'
    adb uninstall "$package"
    adb install "$PREVIOUS_NIGHTLY_APK"
    adb install -r "$candidate"
    check_launch
fi
