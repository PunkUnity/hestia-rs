#!/usr/bin/env bash
# Install what tools/test-apk/build.sh needs into $ANDROID_HOME (default
# ~/Android/Sdk): SDK command-line tools, platform 34, build-tools 34, NDK 30
# (the one CI uses: Slint's prebuilt Skia needs it), plus the Rust target and
# cargo-ndk. JDK 17 comes from the system: on Arch/CachyOS
# `sudo pacman -S --needed jdk17-openjdk rustup`.
set -euo pipefail
: "${ANDROID_HOME:=$HOME/Android/Sdk}"
mkdir -p "$ANDROID_HOME"
cd "$ANDROID_HOME"
if [ ! -x cmdline-tools/latest/bin/sdkmanager ]; then
    curl -fsSL -o cmdline.zip https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip
    rm -rf tmp && unzip -q cmdline.zip -d tmp && mkdir -p cmdline-tools && rm -rf cmdline-tools/latest
    mv tmp/cmdline-tools cmdline-tools/latest && rm -rf tmp cmdline.zip
fi
yes | cmdline-tools/latest/bin/sdkmanager --sdk_root="$ANDROID_HOME" --licenses >/dev/null
cmdline-tools/latest/bin/sdkmanager --sdk_root="$ANDROID_HOME" \
    "platforms;android-34" "build-tools;34.0.0" "ndk;30.0.16248370"
rustup target add aarch64-linux-android
command -v cargo-ndk >/dev/null || cargo install cargo-ndk --locked
echo "Android SDK ready in $ANDROID_HOME"
