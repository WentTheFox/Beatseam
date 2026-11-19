# Phase 0 spike, item 3 (web half): the Rust mixer in an AudioWorklet

Question (docs/rust-gpui-rewrite-plan.md, Phase 0 item 3): can the crossfade mixer we own run in an `AudioWorklet` on
web, including while the tab is in the background?

`src/lib.rs` includes the Windows spike's `mixer.rs` as-is (`#[path]`) and exposes a C ABI (`engine_new`,
`engine_render`, planar output pointers). `processor.js` instantiates the module and calls it once per 128-frame
quantum. Test clips are synthetic sines (220 Hz and 330 Hz) so the output is checkable.

## Run

```text
rustup target add wasm32-unknown-unknown
cd spikes/web-audio
cargo build --release --target wasm32-unknown-unknown
PLAYWRIGHT_DIR=<dir with playwright installed> node run.mjs        # HEADED=1 for a visible, muted browser
```

## Results (Chromium on Windows)

| Check | Result |
| --- | --- |
| Module imports | none. The worklet scope has no `fetch`/`TextDecoder`, so a plain C-ABI module with no wasm-bindgen glue is what works there; the main thread fetches the bytes and passes them in `processorOptions` |
| Module size | 29 KB |
| Offline render through the worklet vs a JS reference of the same formula | max abs error 6e-4 (see below), no dip or bump at the fade: 1 s RMS 0.354 in every window from 3 s to 10 s |
| Cost of a 128-frame quantum | ~0.6 to 1 us on the main thread (20,000 quanta in 12 to 20 ms), against a 2,667 us budget at 48 kHz |
| Realtime `AudioContext` | runs at 48 kHz; the worklet advanced exactly in step with the context clock (8.05 s of audio in 8.05 s) |

The 6e-4 error is the test generator, not the mixer: the Rust sines compute their phase in `f32`, which loses
precision after a few seconds, whereas the JS reference uses `f64`. The real engine reads decoded samples, so this
does not apply, but it is a reminder not to synthesize long signals in `f32`.

## Not verified: background behaviour

The point of the question is whether audio keeps going when the tab is hidden. This could not be tested here:
Playwright keeps every page `visible` (even headed with another tab in front), so no throttling happened (80 timer ticks
in 8 s). What is known from the design rather than from this test: a worklet runs on the audio rendering thread, so it
is not subject to the main-thread timer throttling that hurts the JS app. Whether a hidden tab keeps its `AudioContext`
running depends on the browser (it does while audio is audibly playing in Chromium). To check by hand: serve this folder,
open it, run `start()` in the console, switch tabs for a minute, switch back and compare `probe()` frames with
`ctxTime * 48000`.

## Also not covered

* Decoding in wasm (symphonia compiles to wasm, but not tried here) and loading whole tracks into wasm memory: a
  full track is tens of MB, so real playback should stream decoded chunks to the worklet instead.
* Firefox and Safari.
