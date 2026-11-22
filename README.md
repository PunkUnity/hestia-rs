# Hestia RS test builds (fallback copy)

The main download is the `test-latest` release, built by the fork's "Test APK" workflow:
https://github.com/PunkUnity/hestia-rs/releases/download/test-latest/hestia-rs-test-arm64.apk

This branch keeps a copy of the latest build made in the cloud session:

| Build | Branch | Contents |
| --- | --- | --- |
| 0.1.0-test+105049a (code 468180) | test/android-batch2 | safe-area fix; per-vendor MediaCodec keys (fix/mediacodec-low-latency-keys); test-APK tooling |
| 0.1.0-test.1+3a24e85 (code 2001) | test/android-batch1 | the first test build (shows "local build" in About) |
