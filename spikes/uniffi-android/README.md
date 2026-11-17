# Phase 0 spike, item 5: Compose over a Rust view model through UniFFI

Question (docs/rust-gpui-rewrite-plan.md, Phase 0 item 5): if Android uses Jetpack Compose, does the binding shape
work: a Compose screen observing a Rust view model, with actions flowing back?

* `src/lib.rs`: a `NowPlayingViewModel` object, an immutable `NowPlayingState` record, a `StateListener` callback
  interface the UI implements, action methods (`play_pause`, `select`, `seek`), a paged read (`page`), an error enum,
  an `async fn` (`rescan`), and a Rust thread that advances playback and pushes state.
* `android/`: a Compose app (Material 3) that holds one snapshot in `mutableStateOf`, replaces it on every push, and
  calls the actions from click handlers. A Benchmark button times the boundary.

## Build

```text
rustup target add aarch64-linux-android; cargo install cargo-ndk           # as for spikes/android-gpui
cd spikes/uniffi-android
cargo ndk -t arm64-v8a -P 31 -o android/app/src/main/jniLibs build --release
cargo run --bin uniffi-bindgen -- generate --library target/aarch64-linux-android/release/libbeatseam_vm.so \
    --language kotlin --out-dir android/app/src/main/java
cd android && ./gradlew assembleDebug && adb install -r app/build/outputs/apk/debug/app-debug.apk
```

The generated Kotlin (`uniffi/beatseam_vm/beatseam_vm.kt`) and the `.so` are not committed (see `.gitignore`).
Used: UniFFI 0.32.2, AGP 9.1.0, Compose BOM 2025.09.00, Kotlin compose plugin 2.2.21, JNA 5.17.0.

## Results (Galaxy S24+, Android 17, release Rust library, non-debuggable app)

| Check | Result |
| --- | --- |
| Compose observes Rust state | works: the screen renders from the pushed snapshot; the listener is called with the current state as soon as it is set |
| Pushes from a Rust thread | work; 14 of 16 pushes arrived on a non-main thread. Compose snapshot state is writable from any thread, so no hop to the main thread was needed here |
| Actions back into Rust | work (Play started the Rust ticker and its pushes) |
| Error across the boundary | `select(999999)` throws a typed Kotlin `VmException` |
| Rust `async fn` | becomes a `suspend fun`; `rescan()` suspended 305 ms and returned 20,000 without a runtime |
| Trivial call (`ping`) | 12.9 us (40 to 45 us in a debuggable app: measure release builds only) |
| Full state snapshot with 200 rows | 191 us |
| `page()` of 1,000 / 5,000 / 20,000 rows | 1.2 / 5.2 / 19 ms |
| Size | `libbeatseam_vm.so` 401 KB release (with `opt-level=z`, LTO); `libjnidispatch.so` 177 KB |

What this means for the plan:

* The shape is viable and TalkBack parity comes for free, since the UI is native Compose with its own semantics.
* Push whole snapshots only for small state (a 200-row list every 250 ms is fine). A library of tens of thousands of
  rows has to be paged (`page(offset, limit)`), never put inside the pushed state: 20,000 rows is ~19 ms per crossing.
* Calls are ~13 us each, so a per-frame call or two is fine but chatty per-row calls from a list are not.

## Gotchas

* A Rust `u32` becomes Kotlin `UInt`, which Compose cannot use as a lazy-list key (crash: "Type of the key 0 is not
  supported"); convert with `.toInt()`. Using `i32`/`u64`-as-`i64` ids in the view model avoids it.
* `VmException`'s message prints the fields (`id=999999`), not the Rust `Display` text; map errors to messages in the
  view model if the UI shows them.
* `androidx.activity:activity-compose:1.11.0` requires `compileSdk` 36; 1.10.1 works with 35.
* `uniffi-bindgen` could not run `ktlint` (not installed); the output is unformatted but compiles.

## Not covered

* Streaming a long-lived state from a background service (a MediaSession service holding the view model while the UI is
  gone), lifecycle/process-death handling, and the foreground-service audio that item 3 still has open.
* Accessibility beyond what Compose gives by default (not tested with TalkBack, which the maintainer skipped).
