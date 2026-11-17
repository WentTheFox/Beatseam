# Phase 0 spike: GPUI on Android (community backend, as-is)

Throwaway app for item 2 of Phase 0 in `docs/rust-gpui-rewrite-plan.md`. The same view as `../windows-gpui` (5k-row
list, text input, spinning record + tonearm, slider, all with accessibility attributes), ported to the GPUI snapshot
the Android backend uses.

The backend is [`longbridge/gpui-mobile`](https://github.com/longbridge/gpui-mobile) at `7548ebc4b2`, used with no local
patches. It builds on the `gpui-pre` 0.3.10 snapshots from crates.io, **not** on the Zed git revision the Windows spike
pins, so the two GPUI APIs differ slightly (the one change needed here: `TextElement` needs a hand-written
`IntoElement` impl). The crates.io release of `gpui-mobile` is an empty stub; the real code is only in git.

## Building and running

Toolchain used (all under `D:\Android\`, not in the repo): JDK 17, Android SDK (platform 35, build-tools 35, NDK
27.2.12479018), `rustup target add aarch64-linux-android`, `cargo install cargo-ndk`.

This crate is only the Rust library. It is loaded by the backend's own example host (the Gradle project under
`example/android/gradle` of the cloned backend), which expects the library name `gpui_mobile_example`:

```
git clone https://github.com/longbridge/gpui-mobile && git -C gpui-mobile checkout 7548ebc4b2
cd spikes/android-gpui
cargo ndk -t arm64-v8a -P 31 -o <clone>/example/android/gradle/app/src/main/jniLibs build
cd <clone>/example/android/gradle && ./gradlew assembleDebug
adb uninstall dev.gpui.mobile.example   # see "Gotchas": a plain `install -r` kept running the old example
adb install -g app/build/outputs/apk/debug/app-debug.apk
```

## Results (2026-11-17, Galaxy S24+ SM-S926B, Android 17, debug build)

| Check | Result |
| --- | --- |
| Renders (Vulkan, wgpu) | Yes: list, text field, record + tonearm (animated), slider all draw correctly |
| Soft keyboard + typing | Yes, with a Latin keyboard (Gboard). Focusing the field opens the keyboard on its own; tapping keys put text in the field with a caret. A non-Latin IME was **not** tried. |
| Scrolling | Yes (the 5k list scrolls by swipe) |
| Accessibility tree / TalkBack | **Not verified; skipped by the maintainer.** See below. |

Accessibility, what is known without having run TalkBack:

* The backend does have an Android accessibility bridge at this commit (`src/android/accessibility.rs`, an AccessKit
  `InjectingAdapter` and a Java `AccessibilityDelegate`). Its README is out of date: it still says accessibility is
  incomplete, and its `TODO.md` still lists "assistive technologies support".
* `adb shell uiautomator dump` is **not** a usable probe. It attaches for about a millisecond ("Accessibility
  activated", then "deactivated" in logcat), so it only ever saw the root node. A client that stays attached, such as
  TalkBack, is needed.

## Gotchas

* `adb install -r` of the rebuilt APK kept running the previous example's code; only `adb uninstall` + `install` picked
  up the new native library.
* The phone must be unlocked and awake for the app to render; a locked screen shows a black frame.
* The debug APK is ~1 GB (unstripped native libraries); a release build is far smaller (not measured here).
* The title text overlaps the status bar: the spike doesn't apply the safe-area insets the backend reports.
* License: the backend is "GPL-3.0-or-later OR AGPL-3.0-or-later OR Apache-2.0" in its `Cargo.toml` (GitHub shows
  AGPL-3.0). This repo is MIT, so only the Apache-2.0 option is usable; worth confirming before depending on it.
