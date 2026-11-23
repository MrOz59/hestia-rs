#!/usr/bin/env bash
# Bundle the desktop GUI and PyroWave together. Graphics drivers come from the host.
set -euo pipefail
build=$(realpath "${1:?usage: linux-appimage.sh BUILD_DIR OUTPUT.AppImage}")
output=$(realpath -m "${2:?missing AppImage output path}")
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

curl --fail --location --silent --show-error --retry 3 -o appimagetool \
    https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
echo 'ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0  appimagetool' | sha256sum -c -
chmod +x appimagetool
# Reuse the pinned tool's type-2 runtime so appimagetool never fetches a moving release.
head -c "$(./appimagetool --appimage-offset)" appimagetool > runtime

mkdir -p AppDir/usr/bin AppDir/usr/share/doc/hestia-rs
install -m755 "$build/hestia-app" AppDir/usr/bin/
install -Dm644 "$build/lib/libpyrowave-shared.so.0" AppDir/usr/bin/lib/libpyrowave-shared.so.0
cp -R "$build/lib/licenses" AppDir/usr/share/doc/hestia-rs/
cp "$repo/LICENSE" AppDir/usr/share/doc/hestia-rs/
cp "$repo/packaging/linux/hestia-rs.desktop" AppDir/
cp "$repo/apps/hestia-app/android/res/mipmap-xxxhdpi/ic_launcher.png" AppDir/hestia-rs.png
ln -s usr/bin/hestia-app AppDir/AppRun
mkdir -p "$(dirname "$output")"
ARCH=x86_64 ./appimagetool --appimage-extract-and-run --runtime-file runtime AppDir "$output"

# Check the shipped layout with no mount, display, GPU or system PyroWave.
"$output" --appimage-extract >/dev/null
python3 - <<'PY'
from ctypes import CDLL, POINTER, byref, c_uint32
from pathlib import Path

gui = Path("squashfs-root/AppRun").resolve(strict=True)
assert gui == Path("squashfs-root/usr/bin/hestia-app").resolve()
runtime = CDLL(str(gui.parent / "lib/libpyrowave-shared.so.0"))
runtime.pyrowave_get_api_version.argtypes = [POINTER(c_uint32)] * 3
runtime.pyrowave_get_api_version.restype = None
major, minor, patch = c_uint32(), c_uint32(), c_uint32()
runtime.pyrowave_get_api_version(byref(major), byref(minor), byref(patch))
assert (major.value, minor.value) == (0, 5), "unexpected bundled PyroWave ABI"
PY
