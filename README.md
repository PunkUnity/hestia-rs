# Hestia RS test builds (PunkUnity)

`hestia-rs-test-arm64.apk`: arm64 test build, app ID `com.mroz59.hestia.rs.test`, label "Hestia RS (test)". It installs beside the official nightly. It's signed with a test key, so each test build installs over the previous one.

| Build | Branch | Contents |
| --- | --- | --- |
| 0.1.0-test.1 | test/android-batch1 (54a7bd1) | per-vendor MediaCodec low-latency keys (fix/mediacodec-vendor-keys); pages clear of the system bars (fix/android-safe-area) |
