# Hestia RS test builds (PunkUnity fork)

"Hestia RS (test)" is a separate app (`com.mroz59.hestia.rs.test`), so it installs next to the official nightly. Every build:
- is signed with `test.keystore` (password `android`, alias `androiddebugkey`), a throwaway key for this test app only;
- has a higher version code than the last.

So each build installs over the previous one and keeps its pairing and settings.

## Download on the phone

    https://github.com/PunkUnity/hestia-rs/releases/download/test-latest/hestia-rs-test-arm64.apk

The release notes, and Settings → About in the app ("Hestia RS 0.1.0 (<commit>)"), show which build it is.

## Build

- **On GitHub:** push a `test/*` branch that contains this folder and `.github/workflows/test-apk.yml`. The "Test APK" workflow builds it and replaces the `test-latest` release. Actions must be enabled on the fork (Actions tab, once).
- **On any Linux machine (Haven, a laptop):**

      sudo pacman -S --needed jdk17-openjdk rustup    # Arch/CachyOS; other distros: JDK 17 + rustup
      rustup default stable
      tools/test-apk/setup-android.sh                 # once: SDK, NDK 30, Rust target, cargo-ndk (about 3 GB)
      tools/test-apk/build.sh                         # writes apks/hestia-rs-test-arm64.apk
