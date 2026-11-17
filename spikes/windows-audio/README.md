# Phase 0 spike: audio on Windows (symphonia + cpal + our own mixer)

Throwaway for item 3 of Phase 0 in `docs/rust-gpui-rewrite-plan.md`: decode two local files with `symphonia`, play them
through `cpal`, and crossfade with an equal-power curve computed by **our own mixer** (`src/mixer.rs`), not by an
engine's gain automation.

```
cargo test -p beatseam-spike-windows-audio
cargo run --release -p beatseam-spike-windows-audio -- A.mp3 B.mp3 --fade-at 20 --fade 6 --secs 40 --render out.wav
cargo run --release -p beatseam-spike-windows-audio -- A.mp3 B.mp3 --fade-at 6 --fade 4 --secs 14 --volume 0.2
```

`--render` runs the same mixer offline (no sound) and prints the loudness around the fade; without it the result plays
through the default output device.

## Results (2026-11-17, Windows 11, Realtek speakers via WASAPI shared mode, 48 kHz f32)

| Check | Result |
| --- | --- |
| Mixer unit tests | 5 pass: `a² + b² = 1` on every fade frame, exact endpoints, monotonic and continuous gains, A-to-B switch and silence after the end, output independent of buffer size |
| Decode (symphonia 0.6.1, MP3) | 40 s of a 4.8 MB and a 7.9 MB MP3 in 325 ms / 115 ms (release, files on the Proton Drive folder); 14 s in 14 ms warm |
| Offline render, real tracks | RMS stays smooth through a 6 s fade (no dip, no bump). Peak reached 1.27 mid-fade: two loud tracks summed clip without normalization or a limiter, which the real engine needs |
| Real-time playback (cpal 0.18, WASAPI) | 20.0 s of audio played in 20.4 s wall clock (0.3 s is the deliberate buffer drain); 2036 callbacks; worst callback 17 us, 0.1% of its buffer time; no stream errors |
| Heard by a person | The maintainer listened to the real-time run and it sounded fine (no glitches or level dip reported) |

What the spike is not: resampling is linear (here source and output were both 48 kHz, so it never ran), decoding is
whole-clip rather than streaming, output is f32 only, and there is no normalization gain.

## Not done in this spike (still Phase 0 item 3)

* Android while backgrounded behind a foreground service (needs the Android host from `../android-gpui`, or Compose).
* Web in an `AudioWorklet` with the tab in the background.
