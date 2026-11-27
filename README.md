# Hestia RS

A game-streaming client for [Hermes](https://github.com/MrOz59/Hermes), written
in Rust: a modular core with the whole protocol stack in pure Rust, a UI in
[Slint](https://slint.dev) that sits behind a port (so the toolkit can be
replaced), and Linux on Wayland and Android (phones, tablets, Android TV) as
the first targets.

Hestia RS is made for Hermes. It speaks GameStream because that is what
Hermes speaks today, and uses Hermes' **Hestia protocol v1** extensions on
top of it: capabilities, session preparation, virtual displays, diagnostics,
clipboard and server commands. A protocol of its own, sturdier and faster
over WAN, is a goal for the distant future; until then, GameStream it is.

Sunshine and Apollo speak GameStream too, so Hestia RS may connect to them,
but only as a side effect: they aren't a target, nothing is tested against
them, and features are designed around what Hermes offers. Likewise on the
desktop, the target is **Wayland**; X11 isn't one.

> **Status: prototype.** It streams from Hermes hosts on Linux and Android
> (nightly builds below). Noticeable lag remains and is being studied: the
> stream reports and the live diagnosis exist to pin it down. See
> [docs/ROADMAP.md](docs/ROADMAP.md).

## Download

Every push to `main` that passes CI replaces the **`nightly`** pre-release
(Releases page) with:

| File | For |
| --- | --- |
| `hestia-rs-android-universal.apk` | any Android device: phones, tablets, Android TV, Chromebooks (all four ABIs) |
| `hestia-rs-android-<abi>.apk` | a smaller APK for one ABI (most phones: `arm64-v8a`) |
| `Hestia-RS-x86_64.AppImage` | Linux GUI with the PyroWave runtime in one executable (same system requirements as the tarball) |
| `hestia-rs-linux-x86_64.tar.gz` | desktop Linux on Wayland (needs libopus, ALSA, libva, libgbm and a recent glibc) |
| `ui-screenshots.zip` | the screens, rendered headless (phone and desktop) |
| `SHA256SUMS` | checksums |

On Linux, run `chmod +x Hestia-RS-x86_64.AppImage` once, then launch
`./Hestia-RS-x86_64.AppImage` to open the GUI. If FUSE is unavailable, use
`./Hestia-RS-x86_64.AppImage --appimage-extract-and-run`.

With the tarball, extract the entire directory and launch `./hestia-rs/hestia-app`.
Keep its sibling `lib/` directory with the binaries when moving them. The
`hestia` executable is the CLI.

The release notes list the commits since the previous nightly; Settings →
About in the app shows which commit a build is.

Nightly APKs are always signed with the same certificate, so each one installs
over the previous one. Its SHA-256 fingerprint, as `apksigner verify
--print-certs` shows it:

```
a76f6edbd5bfffbc6953f90673b8487c6029398db16f67ae9f6b2da84a2f6b0b
```

## Features

### Hosts and pairing

- **Discovery** on the LAN over mDNS (`_nvstream._tcp`), or add a host by IP
  or name. **Wake-on-LAN**: tapping an offline host wakes it (once its MAC
  address is known) and waits for it to come up.
- **Pairing** with the classic client PIN (typed into the Hermes web UI) or
  with Hermes' **OTP** (PIN and passphrase shown by the host).
- **One host, every address**: the pairing belongs to the host, not to an IP.
  The same PC reached over the LAN, over a VPN such as Tailscale, by name or
  at a new DHCP address stays one paired host. Each check asks all of its
  addresses at once and uses the LAN when you're on it.
- **IPv6 first**: on dual-stack networks IPv6 is preferred, unique-local
  addresses first, and an IPv6 global address of your own network counts as
  LAN. Discovery and Wake-on-LAN also work on IPv6-only networks. Connections
  race IPv6 and IPv4 (Happy Eyeballs), so a broken IPv6 path costs a quarter
  of a second, not a timeout.
- **Apps**: list with box art and the running app marked; launch, resume and
  quit; "back" while starting cancels the launch.

### Hermes

- **Session preparation** before launch: virtual display size, HDR, scale
  factor, Gamescope session.
- **Host tools**: display status and recovery, server commands (with
  confirmation where Hermes asks for one), clipboard send/fetch, this
  device's permissions, diagnostics, the readiness preflight (failures first,
  also summarised on the host card), runtime status (encoder, sessions,
  pipeline), the full diagnostic report and a shortcut to the web UI.
- **Stream provenance**: after the first frame, a banner shows what the host
  actually uses (GPU, encoder, capture path); it goes into the report too.
- **Clipboard sync** (Settings → Clipboard sync): while streaming, what's
  copied on either side can be pasted on the other; the stream menu also
  sends or fetches it once. Android reads the clipboard only when it changed;
  Linux uses wl-clipboard or xclip.

### Streaming

- **Protocol** in pure Rust: RTSP handshake (plain and `rtspenc://`), SDP,
  AES-GCM control stream over a built-in ENet client, video RTP reassembly
  with Reed–Solomon FEC (byte-compatible with nanors), IDR recovery, a video
  watchdog, Opus audio (AES-CBC), and input packets (keyboard, text, mouse,
  scroll, touch, pen, gamepad).
- **Video** ([docs/VIDEO.md](docs/VIDEO.md) has the whole pipeline):
  - *Android*: MediaCodec (H.264, HEVC, AV1) renders into GPU buffers the UI
    draws directly, with no CPU copy (Settings → Zero-copy video). Without
    it, pictures are read back on the CPU; OpenH264 is the last fallback.
  - *Linux desktop*: VA-API (H.264, HEVC, AV1) decodes into GPU memory the
    window draws in place, shared as DMA-BUFs: no copy at all (Settings →
    GPU colour conversion). Without EGL DMA-BUF import the pictures are
    copied out, and without a usable VA-API device (or with Settings →
    Hardware decoding off) OpenH264 decodes H.264 in software, with the GPU
    converting its YUV planes to RGB while drawing them, or the CPU when
    that's off or unavailable.
  - Decoding runs on its own thread and never drops a compressed frame for
    being late (dropping one would cost a keyframe): bursts decode through
    and only the newest picture is shown. Keyframe requests back off when
    they don't fix a failing decoder.
- **Audio**: stereo, 5.1 and 7.1 Opus. On Linux, surround uses the speaker
  layout reported by the default ALSA device. On Android, AAudio uses
  positional surround on Android 12L (API 32) and newer. Playback falls
  back to 5.1 or stereo when the requested layout cannot be opened; Android
  can also mix the stream to fit the current route.

### Input

- **Keyboard** (US-layout key mapping, text for other characters), absolute
  **mouse**, buttons and scroll. `Ctrl+Alt+Shift+Q` disconnects,
  `Ctrl+Alt+Shift+S` toggles statistics.
- **Touch modes** (stream menu, remembered): *trackpad* (default; tap
  clicks, hold right-clicks, tap then drag drags), *mouse* (the cursor goes
  where you touch) and *touchscreen* (every finger reaches the host as a real
  touch). Two-finger tap right-clicks, two-finger drag scrolls, three-finger
  tap opens the keyboard.
- **On-screen controller** (stream menu, remembered): sticks (tap for
  L3/R3), d-pad, A/B/X/Y, bumpers, analog triggers, Back/Home/Start, every
  finger independent; under the picture in portrait. It takes a controller
  slot like a physical one.
- **Game controllers**: up to four, with rumble. On Android, rumble goes to
  the controller's motor or the phone's. On desktop Linux, controllers are
  read straight from evdev (no SDL or libudev), with force feedback and hot
  plugging. Outside a stream (Android) they navigate the app: A confirms, B
  goes back.
- **Keyboard bar** in the stream menu: a text field (opens the on-screen
  keyboard) typed on the host, plus Esc, Tab, Enter, Backspace, Win,
  Alt+Tab and Alt+F4.

### Stream screen

- Picture scale (fit, fill or stretch), pinch zoom up to 4× with two-finger
  pan, full screen, orientation (Android: landscape, portrait or the
  device's), an optional menu button.
- On supported Android phones and tablets, going Home while streaming opens
  a picture-in-picture window with video and audio. Expanding it returns to
  the same stream. Closing it disconnects and leaves the game running on the host.
- Statistics overlay, "Fix picture" (asks for a fresh keyframe), disconnect
  or quit the app.
- Android asks before a stream when Bluetooth is on and the Wi-Fi is on
  2.4 GHz: the two share that band, and Bluetooth headphones in particular
  make the picture stutter and the sound drop out. Streaming over the
  router's 5 GHz network, leaving 2.4 GHz to Bluetooth, avoids it. The
  session report says when Bluetooth was on and where the audio played.

### Settings

- **Presets**: Fast (≤1080p60), Balanced (native, ≤60 fps), Quality (native
  resolution and refresh, more bitrate) and Battery saver (720p30), computed
  from this device's display (EDID on Linux, `Display.Mode` on Android) with
  the bitrate table of [Hestia](https://github.com/MrOz59/Hestia), the
  Moonlight-Qt-based desktop client. Editing resolution, frame rate or bitrate
  by hand switches to Custom; **custom resolution** takes any size (e.g.
  2560x1080).
- Codec, bitrate, HDR, audio layout, virtual display, Gamescope session,
  audio on the host, game optimisation, clipboard sync, decoding options.
- **Stream profiles**: the video, audio and host options saved under a name
  ("Couch TV", "Phone 720p"); each host can launch with one (its page →
  Stream profile), while touch, orientation and decoding stay this device's.

### Diagnostics

- **Live diagnosis**: every second the client works out whether the network,
  the decoder, the display or the host is the bottleneck (Hestia's
  classifier). The overlay says which and how often it happened in the last
  five minutes; the report marks each bad second.
- **Stream reports**: every session, including one that fails to start,
  writes a report with the client, host and launch details, what was
  negotiated, events, decoder and video latency figures, per-second
  statistics and every log line from launch to end. It opens when the stream
  ends (Settings → Diagnostics turns that off, or reopens the last one) and
  can be shared (Android share sheet), opened in the file manager (desktop)
  or sent to the host's clipboard. Files go to
  `Android/data/com.mroz59.hestia.rs/files/reports/` on Android and
  `~/.local/share/hestia-rs/reports/` on Linux (the CLI writes there too).
- **App log** (Settings → Diagnostics): everything logged since the app
  started, with the hosts' last known state, shared the same way.
- **Debug options**: tap Settings → About seven times (as with Android's
  developer options) to show a Debug section at the end of Settings. It
  previews the launch screen without a stream until Back, holds the launch
  animation still, keeps every library's Debug lines in the app log and
  reports (verbose log), and shows this device's details, display and
  decoders, copied with a tap. It stays until hidden again.

### Platforms and UI

- **Desktop Linux on Wayland** and **Android** 8+ (NativeActivity,
  universal APK).
- **Android TV**: launcher banner and entry, nothing touch-only, and the
  whole app works with a remote. The arrows move an ember focus glow over
  every control, left/right change a settings row's value, and the focused
  control always scrolls into view.
- **UI**: a dark, warm theme with an ember-orange accent and the bundled
  Outfit typeface (SIL OFL, `apps/hestia-app/fonts`), with layouts for
  phones, TVs and desktop windows.
- **Swappable toolkit**: everything but drawing lives in `hestia-frontend`,
  behind a small port. Slint is one shell; another toolkit (egui, Qt…) would
  be another, with no change to the app
  ([docs/FRONTENDS.md](docs/FRONTENDS.md)).
- **Slint patch**: `third_party/i-slint-backend-android-activity` is Slint's
  Android backend with an input hook (controllers), a fix for mouse-wheel
  scrolling (it crashed upstream) and remote/controller keys for the UI (see
  its `PATCHES.md`).
- **cros-codecs patch**: `third_party/cros-codecs` is ChromeOS's VA-API
  decoders with a low-delay mode (each picture comes out as soon as it is
  decoded) and a build fix for libva 2.23 (see its `PATCHES.md`).
- A **`hestia` CLI** for scripting and testing hosts.

### Known limits

- **Lag**: streams from Hermes work but show noticeable latency; finding where
  it comes from is the current priority.
- **Hosts other than Hermes** (Sunshine, Apollo): not a target. They may
  work because they share GameStream, but they aren't tested and problems
  that only show up with them aren't a priority.
- **X11**: not a target. Slint's window backend can still open on X11, but
  nothing is tested or tuned there. Desktop features are built for Wayland:
  the clipboard and zero-copy video today (on X11 with GLX, hardware-decoded
  pictures are copied), pointer capture next.
- **VA-API is new** and hasn't been tried on real hardware yet. It needs a
  driver that can import its own surfaces back (Intel's media driver and
  Mesa's for AMD and Intel): NVIDIA's VA-API driver can't, so NVIDIA
  desktops decode H.264 in software for now, where a 4K or high-frame-rate
  stream may be more than one CPU core can handle. 8-bit only (no 10-bit
  HEVC or AV1 yet).
- No HDR presentation (10-bit) yet.
- Keys are mapped by the character they produce (US layout), not by physical
  scancode; the desktop mouse is absolute only (no pointer capture); no
  stylus support yet.

## Layout

```
crates/
  hestia-crypto      identity (RSA-2048 + self-signed cert), pairing crypto — pure Rust
  hestia-nvhttp      GameStream HTTP(S): serverinfo, pairing, applist, box art, launch/quit
  hestia-api         Hermes "Hestia protocol v1" client (/api/hestia/v1/*)
  hestia-discovery   mDNS discovery (legacy-unicast queries, no multicast lock needed)
  hestia-core        UI-agnostic facade: identity, host store, settings, pairing, apps, launch, reports
  hestia-stream      streaming data plane: RTSP, ENet control, video + FEC, audio, input
  hestia-media       decoders and players behind VideoSink/AudioSink (MediaCodec, VA-API, OpenH264, Opus)
  hestia-frontend    the GUI without a toolkit: controller, input, platform services, video presentation
apps/
  hestia-app         the Slint shell over hestia-frontend (desktop binary + Android cdylib)
  hestia-cli         `hestia` command-line client
third_party/         Slint's Android backend and cros-codecs (VA-API decoding), patched
tools/               ENet interop test, FEC reference vectors
scripts/             Android packaging and install checks
```

Each crate has one job and depends only on the layers below it. More in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Building

Rust 1.95+ (edition 2024; Slint 1.19, and with it the patched Android
backend in `third_party`, needs 1.95).

### Linux

```sh
# Debian/Ubuntu: Slint, audio (Opus, ALSA), a C++ compiler and nasm for OpenH264,
# libva, libgbm and libclang for VA-API
sudo apt install build-essential nasm libfontconfig1-dev libxkbcommon-dev libwayland-dev \
    libopus-dev libasound2-dev libva-dev libgbm-dev libdrm-dev libclang-dev
# Arch/CachyOS
sudo pacman -S --needed base-devel nasm fontconfig libxkbcommon wayland opus alsa-lib \
    libva mesa clang
# Fedora
sudo dnf install gcc-c++ nasm fontconfig-devel libxkbcommon-devel wayland-devel \
    opus-devel alsa-lib-devel libva-devel mesa-libgbm-devel libdrm-devel clang-devel

cargo run --release -p hestia-app      # GUI
cargo run --release -p hestia-cli -- --help
```

VA-API decoding (feature `vaapi`, on by default) generates its libva
bindings at build time, hence libclang, and links libva, libva-drm and libgbm.
Without them: `cargo build --release -p hestia-app --no-default-features
--features openh264` (software decoding only). The decoder itself is
cros-codecs, vendored with low-delay changes in `third_party/cros-codecs`.

`nasm` assembles OpenH264's x86 SIMD code. Without it the build still
works, but the software decoder is about a quarter slower: the build prints
a warning and the decoder calls itself "OpenH264 (software, no SIMD)" in the
stats and reports. Installed it afterwards? `cargo clean -p openh264-sys2`
and build again.

Build with `--release` to measure anything: debug builds compile OpenH264
and the colour conversion without optimisation.

### Android

Use **NDK r30** (or whichever NDK matches skia-bindings' prebuilt Skia, see
the CI workflow): Slint renders with Skia on Android, and an older NDK's libc++
lacks symbols the prebuilt library needs, so the app builds but fails to load
on the device. Point `ANDROID_NDK_HOME` / `ANDROID_NDK_ROOT` at it, and have
a JDK and an SDK platform installed: the build compiles two small Java
helpers (Slint's and `crates/hestia-frontend/java`). For the x86_64 ABI,
install `nasm` too (OpenH264's SIMD code, as on the desktop); 32-bit x86
builds without it, since that assembly is not position independent and
Android refuses text relocations.

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk cargo-apk
# native library and APK with picture-in-picture (set ANDROID_KEYSTORE and
# ANDROID_KEYSTORE_PASSWORD to sign it for installation)
cargo ndk -t arm64-v8a --platform 26 -o target/android-libs build --release -p hestia-app --lib
scripts/android-package.sh target/android-libs target/android-apks 1 0.1.0-dev
# or a quick arm64 APK with the stock NativeActivity
cargo apk build --release -p hestia-app --lib
```

Release APKs come from `scripts/android-package.sh` (aapt2, zipalign,
apksigner) with `apps/hestia-app/android/` (manifest with the Android TV
entry and banner, icons): it takes `<abi>/libhestia_app.so` for any of
arm64-v8a, armeabi-v7a, x86_64 and x86 and makes one universal APK plus one
per ABI. CI builds the four ABIs (the 32-bit ones compile Skia from source,
which has no prebuilt binaries for them), checks that each library has no
unresolved C++ runtime symbols, packages them, and installs the universal
APK on an Android 16 emulator and checks that the PiP activity starts, both
fresh and after updating from the previous nightly.

## CLI

```sh
hestia identity                 # data dir, unique id, certificate fingerprint
hestia discover [--seconds 3] [--save]   # scan the LAN (and remember what's found)
hestia add 192.168.0.10         # add by address (IP or name, optionally :port)
hestia list                     # known hosts
hestia status mypc              # online/paired/codecs + Hermes capabilities
hestia pair mypc                # prints a PIN; enter it in Hermes → PIN
hestia pair mypc --otp 1234 --passphrase secret     # Hermes OTP pairing
hestia apps mypc
hestia launch mypc Desktop --width 2560 --height 1440 --fps 120
hestia stream mypc Desktop --width 1280 --height 720 --bitrate 20000 --audio    # headless stream + stats
hestia stream mypc Desktop --snapshot frame.ppm --seconds 5      # save a decoded frame
hestia stream mypc Desktop --dump capture --seconds 10           # raw capture.h264/.h265/.obu
hestia stream mypc Desktop --count --quit                        # count frames, quit the app after
hestia quit mypc
hestia forget mypc              # forget the host locally
# Hermes
hestia diagnostics mypc         # diagnostics JSON
hestia host-report mypc [-o FILE|-]    # full diagnostic report (default: data dir/reports)
hestia permissions mypc         # this device's permissions
hestia commands mypc            # server commands
hestia run mypc "Restart Steam" [--yes]
hestia clipboard mypc [--set "text"]
hestia display mypc [--recover [--force]]
hestia web mypc                 # the web UI's address
```

The CLI and the GUI share the same data directory (`~/.local/share/hestia-rs`,
overridable with `HESTIA_DATA_DIR`), so pairing in one shows up in the other.
Overrides given to `launch`/`stream` (`--width`, `--height`, `--fps`, and
`--bitrate` for `stream`) apply to that session only; the saved settings are
not changed.

## Tests and CI

`cargo test --workspace` runs, among others:

- an end-to-end session against a mock host (RTSP, ENet control,
  FEC-protected video with losses);
- a decode of a real H.264 clip, to RGBA and to planes for the GPU;
- the colour converters against the reference formula;
- the desktop GPU colour pass in headless Mesa contexts (OpenGL ES 3, OpenGL
  core and compatibility; skipped where there is no EGL), for planes and for
  NV12 pictures in EGL images (VA-API's path, from GL textures there).

`tools/enet-interop/run.sh` checks the ENet client against the C ENet that
hosts use.

CI (`.github/workflows/ci.yml`) runs:

- formatting, clippy, the tests, a release build and headless UI screenshots
  on Linux;
- the ENet interop test;
- the four Android ABIs, then packaging and the emulator install/update test;
- on `main`, the nightly release.

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): the crates and how they fit.
- [docs/VIDEO.md](docs/VIDEO.md): the video pipeline per platform, its costs,
  and how to read a stream report's video figures.
- [docs/FRONTENDS.md](docs/FRONTENDS.md): the UI port, and how to write a
  shell for another toolkit.
- [docs/ROADMAP.md](docs/ROADMAP.md): what's done and what's next.
- [docs/THEME-PACKS.md](docs/THEME-PACKS.md) explains how to make and share a palette ZIP.

## License

GPL-3.0, like Hermes and the Moonlight protocol code it follows.
