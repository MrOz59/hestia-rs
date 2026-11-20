#!/usr/bin/env bash
set -euo pipefail

candidate=$1
package=com.mroz59.hestia.rs

echo 'Checking fresh install of the universal APK'
adb install "$candidate"

if [[ -n "${PREVIOUS_NIGHTLY_APK:-}" ]]; then
    echo 'Checking update from the previously published universal APK'
    adb uninstall "$package"
    adb install "$PREVIOUS_NIGHTLY_APK"
    adb install -r "$candidate"
fi
