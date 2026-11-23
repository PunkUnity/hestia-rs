#!/usr/bin/env bash
# Packages Hestia RS for Android: one universal APK with every ABI built,
# plus one APK per ABI (smaller downloads), all signed with the same key so
# they install over each other.
#
#   scripts/android-package.sh <libs-dir> <out-dir> <version-code> <version-name>
#
# <libs-dir>/<abi>/libhestia_app.so for any of arm64-v8a, armeabi-v7a,
# x86_64, x86. Needs a JDK, ANDROID_HOME (build-tools 34, platform 34) and, to
# sign, ANDROID_KEYSTORE (path) + ANDROID_KEYSTORE_PASSWORD; the key alias
# is ANDROID_KEY_ALIAS when the keystore holds more than one, and the key's
# own password ANDROID_KEY_PASSWORD when it differs from the store's.
set -euo pipefail

libs=$(realpath "$1")
out=$(mkdir -p "$2" && realpath "$2")
version_code=$3
version_name=$4

here=$(cd "$(dirname "$0")/.." && pwd)
android="$here/apps/hestia-app/android"
build_tools="$ANDROID_HOME/build-tools/34.0.0"
android_jar="$ANDROID_HOME/platforms/android-34/android.jar"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

abis=()
for abi in arm64-v8a armeabi-v7a x86_64 x86; do
    if [ -f "$libs/$abi/libhestia_app.so" ]; then
        abis+=("$abi")
    fi
done
if [ ${#abis[@]} -eq 0 ]; then
    echo "no libhestia_app.so under $libs/<abi>/" >&2
    exit 1
fi
echo "ABIs: ${abis[*]}"

# The launch activity and the pairing service must be available to the
# Android framework before it loads the native library. The two JNI helper
# dex files stay inside the .so.
mkdir -p "$work/classes"
javac --release 8 -classpath "$android_jar" -d "$work/classes" "$android"/java/*.java
"$build_tools/d8" --lib "$android_jar" --min-api 26 --output "$work" \
    "$work"/classes/com/mroz59/hestia/rs/*.class

"$build_tools/aapt2" compile --dir "$android/res" -o "$work/res.zip"
"$build_tools/aapt2" link -o "$work/base.apk" -I "$android_jar" \
    --manifest "$android/AndroidManifest.xml" \
    --min-sdk-version 26 --target-sdk-version 34 \
    --version-code "$version_code" --version-name "$version_name" \
    "$work/res.zip"
(cd "$work" && zip -q -0 base.apk classes.dex)

sign() {
    local unsigned=$1 signed=$2
    "$build_tools/zipalign" -f 4 "$unsigned" "$work/aligned.apk"
    if [ -n "${ANDROID_KEYSTORE:-}" ]; then
        local alias_args=()
        if [ -n "${ANDROID_KEY_ALIAS:-}" ]; then
            alias_args=(--ks-key-alias "$ANDROID_KEY_ALIAS")
        fi
        if [ -n "${ANDROID_KEY_PASSWORD:-}" ]; then
            alias_args+=(--key-pass env:ANDROID_KEY_PASSWORD)
        fi
        "$build_tools/apksigner" sign --ks "$ANDROID_KEYSTORE" --ks-pass env:ANDROID_KEYSTORE_PASSWORD \
            "${alias_args[@]}" --out "$signed" "$work/aligned.apk"
        "$build_tools/apksigner" verify "$signed"
    else
        echo "::warning::ANDROID_KEYSTORE not set: $(basename "$signed") is unsigned"
        mv "$work/aligned.apk" "$signed"
    fi
}

# One APK with `abis` (the native libraries compressed: the manifest asks
# Android to extract them, keeping downloads small).
package() {
    local name=$1
    shift
    rm -rf "$work/stage"
    mkdir -p "$work/stage/lib"
    for abi in "$@"; do
        mkdir -p "$work/stage/lib/$abi"
        cp "$libs/$abi/libhestia_app.so" "$work/stage/lib/$abi/"
        if [[ -f "$libs/$abi/libpyrowave-shared.so" ]]; then
            cp "$libs/$abi/libpyrowave-shared.so" "$work/stage/lib/$abi/"
            mkdir -p "$work/stage/assets/pyrowave"
            cp -R "$libs/$abi/licenses" "$work/stage/assets/pyrowave/"
        fi
    done
    cp "$work/base.apk" "$work/unsigned.apk"
    (cd "$work/stage" && zip -q -r -9 "$work/unsigned.apk" .)
    sign "$work/unsigned.apk" "$out/$name"
    echo "$name: $(du -h "$out/$name" | cut -f1) ($*)"
}

package hestia-rs-android-universal.apk "${abis[@]}"
for abi in "${abis[@]}"; do
    package "hestia-rs-android-$abi.apk" "$abi"
done
