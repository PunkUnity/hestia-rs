#!/usr/bin/env bash
# Build an arm64 "Hestia RS (test)" APK from the current checkout.
#
# The test app has its own ID (com.mroz59.hestia.rs.test), so it installs
# beside the official nightly, and every build is signed with the same test
# key (test.keystore here) with a higher version code, so it installs over
# the previous test build and keeps its pairing and settings.
#
# Needs the Android SDK and NDK (setup-android.sh), JDK 17, rustup with the
# aarch64-linux-android target, and cargo-ndk.
#
# Usage: tools/test-apk/build.sh [output-dir]   (default: apks/)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out=$(mkdir -p "${1:-$root/apks}" && cd "${1:-$root/apks}" && pwd)

: "${ANDROID_HOME:=$HOME/Android/Sdk}"
ndk_version=30.0.16248370
export ANDROID_HOME
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/$ndk_version"
export ANDROID_NDK_ROOT=$ANDROID_NDK_HOME ANDROID_NDK=$ANDROID_NDK_HOME NDK_HOME=$ANDROID_NDK_HOME
export ANDROID_JAR="$ANDROID_HOME/platforms/android-34/android.jar"
# d8 from build-tools 34 can't read class files from newer JDKs.
if [ -z "${JAVA_HOME:-}" ]; then
    for jdk in /usr/lib/jvm/java-17-openjdk /usr/lib/jvm/java-17-openjdk-amd64; do
        [ -d "$jdk" ] && export JAVA_HOME=$jdk && break
    done
fi
[ -n "${JAVA_HOME:-}" ] && export PATH="$JAVA_HOME/bin:$PATH"
for need in "$ANDROID_NDK_HOME" "$ANDROID_JAR" "$ANDROID_HOME/build-tools/34.0.0"; do
    [ -e "$need" ] || { echo "missing $need: run tools/test-apk/setup-android.sh" >&2; exit 1; }
done

cd "$root"
commit=$(git rev-parse --short=7 HEAD)
branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = HEAD ] && branch=${GITHUB_REF_NAME:-detached}
# Settings → About shows "Hestia RS <version> (<commit>)".
export HESTIA_BUILD_ID=$commit
cargo ndk -t arm64-v8a --platform 26 build --release -p hestia-app --lib

target_dir=${CARGO_TARGET_DIR:-$root/target}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/jni/arm64-v8a" "$work/pkg/scripts" "$work/pkg/apps/hestia-app"
cp "$target_dir/aarch64-linux-android/release/libhestia_app.so" "$work/jni/arm64-v8a/"
cp scripts/android-package.sh "$work/pkg/scripts/"
cp -r apps/hestia-app/android "$work/pkg/apps/hestia-app/"
sed -i 's|package="com.mroz59.hestia.rs"|package="com.mroz59.hestia.rs.test"|; s|android:label="Hestia RS"|android:label="Hestia RS (test)"|' \
    "$work/pkg/apps/hestia-app/android/AndroidManifest.xml"

version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
# Minutes since 2026-01-01: always higher than the last build's.
code=$(( ($(date +%s) - 1767225600) / 60 ))
export ANDROID_KEYSTORE="$here/test.keystore" ANDROID_KEYSTORE_PASSWORD=android
bash "$work/pkg/scripts/android-package.sh" "$work/jni" "$work/apks" "$code" "$version-test+$commit"
cp "$work/apks/hestia-rs-android-arm64-v8a.apk" "$out/hestia-rs-test-arm64.apk"
printf '%s\n' "Hestia RS (test) $version-test+$commit, branch $branch, version code $code" | tee "$out/hestia-rs-test-arm64.txt"
echo "APK: $out/hestia-rs-test-arm64.apk"
