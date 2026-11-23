#!/usr/bin/env bash
# Build the same wire format and C ABI as the Hermes encoder.
set -euo pipefail
platform=${1:?usage: build-pyrowave.sh linux|arm64-v8a|x86_64 output-directory}
out=$(mkdir -p "${2:?missing output directory}" && realpath "$2")
work=$(dirname "$0")/../target/pyrowave
mkdir -p "$work"
work=$(realpath "$work")
revision=d2997ac172bdc00e29c58e3f2938acb7e94580bf
fetch() {
    local path=$1 remote=$2 commit=$3 top
    # Only run git in a directory that is its own repository. A cache restore
    # can leave a broken .git behind, and git would then walk up to the
    # enclosing checkout and check this commit out over it.
    top=$(git -C "$path" rev-parse --show-toplevel 2>/dev/null || true)
    if [[ -z $top || $top != "$(cd "$path" && pwd -P)" ]]; then
        rm -rf "$path"
        git init -q "$path"
    fi
    if [[ $(git -C "$path" rev-parse HEAD 2>/dev/null || true) != "$commit" ]]; then
        git -C "$path" fetch --depth 1 "$remote" "$commit"
        git -C "$path" checkout --detach FETCH_HEAD
    fi
}
src=$work/src
fetch "$src" https://github.com/Themaister/pyrowave.git "$revision"
granite=$(sed -n 's/^GRANITE_COMMIT=//p' "$src/checkout_granite.sh")
[[ $granite =~ ^[a-f0-9]{40}$ ]]
fetch "$src/Granite" https://github.com/Themaister/Granite.git "$granite"
git -C "$src/Granite" submodule update --init --depth 1 third_party/volk third_party/khronos/vulkan-headers
args=()
case "$platform" in
    linux) lib=libpyrowave-shared.so.0 ;;
    arm64-v8a|x86_64)
        lib=libpyrowave-shared.so
        args+=("-DCMAKE_TOOLCHAIN_FILE=${ANDROID_NDK_HOME:?}/build/cmake/android.toolchain.cmake"
            "-DANDROID_ABI=$platform" -DANDROID_PLATFORM=android-26 -DANDROID_STL=c++_static
            "-DCMAKE_SHARED_LINKER_FLAGS=-Wl,-z,max-page-size=16384")
        ;;
    *) echo "Unsupported PyroWave target: $platform" >&2; exit 1 ;;
esac
build=$work/$platform
cmake -S "$src" -B "$build" -DCMAKE_BUILD_TYPE=Release -DPYROWAVE_DEVEL=OFF -DPYROWAVE_UTILS=OFF "${args[@]}"
cmake --build "$build" --target pyrowave-shared --parallel "${CMAKE_BUILD_PARALLEL_LEVEL:-2}"
install -m 755 "$build/$lib" "$out/$lib"
if [[ $platform != linux ]]; then
    tools=$(dirname "$(find "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" -name llvm-readelf -print -quit)")
    "$tools/llvm-strip" --strip-unneeded "$out/$lib"
    dynamic=$("$tools/llvm-readelf" -d "$out/$lib")
    [[ $dynamic == *"[$lib]"* && $dynamic != *"[libc++_shared.so]"* ]]
fi
mkdir -p "$out/licenses"
for project in "$src" "$src/Granite" "$src/Granite/third_party/volk" "$src/Granite/third_party/khronos/vulkan-headers"; do
    name=$(basename "$project")
    [[ $name != src ]] || name=PyroWave
    for file in "$project"/LICENSE*; do
        [[ -f "$file" ]] && cp "$file" "$out/licenses/$name-$(basename "$file")"
    done
done
if [[ -d "$src/Granite/third_party/khronos/vulkan-headers/LICENSES" ]]; then
    cp -R "$src/Granite/third_party/khronos/vulkan-headers/LICENSES" "$out/licenses/vulkan-headers"
fi
