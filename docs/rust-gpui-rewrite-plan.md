# Rewriting the Beatseam apps in Rust with GPUI

Status: **proposal / plan**. Nothing in this document is implemented yet. It is the starting point for discussion. The
phases below are ordered so that each one ends in something shippable or in a clear go/no-go decision, and the existing
React Native apps keep shipping until each platform's Rust app reaches parity.

## Goals

* One Rust codebase for the client apps on **Android, Windows and web**. Desktop Linux and macOS come along almost for
  free, since they are GPUI's best-supported targets.
* Share as much code as possible: playback, crossfade, analysis, library/playlist/lyrics logic, server-mode sync,
  scrobbling *and* the UI itself. Per-platform code should be limited to thin adapters (file access, OS media
  controls, secret storage, background execution), the same way `apps/mobile/src/adapters/*` works today but smaller.
* Fix structural problems the current stack works around rather than solves (see "What we expect to gain").
* Keep the wire format with the server compatible in both directions throughout (`docs/api-compatibility.md` still
  applies; a Rust client is just another app release).

## Non-goals (for now)

* Rewriting `apps/server`. The NestJS server stays as is during the client rewrite. It already decodes with ffmpeg in
  worker threads and is not the source of the platform pain. Porting it can be reconsidered once the client crates
  exist, since the scanner/analysis code would then be shared Rust (see "Later: the server").
* iOS. Not supported today. The architecture below would not prevent it (the community mobile backend targets iOS too),
  but it is not planned.
* New features. Parity first. The open TODOs in `CLAUDE.md` move over as TODOs, except where the new architecture
  closes them as a side effect (listed below).

## Where we are today

| Area | Today | Size |
| --- | --- | --- |
| `packages/core` | Playback (`PlaylistPlayer`, `TrackPlayer`, preload scheduler), crossfade planning, BPM/loudness/silence analysis, library scan, m3u8, lyrics matching/LRC, metadata (jsmediatags), offline library, scrobbling, Discord presence protocol, server mode (sync, mutation queue), Redux store | ~21k lines TS, 59 test files (~9.6k lines) |
| `packages/ui` | Shared React Native components and hooks (library, now playing, lyrics editor/sync, settings, server, onboarding) | ~13k lines |
| `packages/api` | zod schemas for the `/api/v1` wire format, `API_VERSION` | ~1.2k lines |
| `apps/mobile` | RN app for Android + react-native-windows; adapters per platform; Kotlin native modules (SAF file access, offline downloads, keystore, background ticks, Discord binder RPC, app update); C++/WinRT modules on Windows (audio engine, file access, cover resizer) | `App.tsx` ~1.3k lines + ~2.3k adapters + native |
| `apps/web` | Vite + react-native-web; Web Audio engine, File System Access API, IndexedDB, OPFS offline backend, Media Session | `App.tsx` ~1.3k lines + ~2.7k adapters |

Platform seams that already exist and map directly onto Rust traits: `AudioEngine` (`packages/core/src/audio-engine/types.ts`),
`FileAccess` (`packages/core/src/file-access/types.ts`), `LibraryStore` (`packages/core/src/library-store/types.ts`),
the offline backend (`packages/core/src/offline/types.ts`), cover art resizing, secret storage, media session, Discord
RPC transport, background ticks and app update.

## The state of GPUI on our target platforms (checked November 2026)

This is the biggest risk in the whole plan, so it comes first.

| Platform | GPUI support | Notes |
| --- | --- | --- |
| Windows | **Official** (upstream Zed) | Zed ships on Windows. DirectX renderer. |
| Linux / macOS | **Official** | Not targets today, but free once desktop works. |
| Android | **Community only** | `gpui-mobile` (iOS via Metal, Android via Vulkan/GL through wgpu; several forks incl. `longbridge/gpui-mobile`). Self-described as early development; arm64 tested, other ABIs untested; mobile IME composition, accessibility and lifecycle hooks are listed as incomplete. Zed's own "gpui: On Android" issue (zed-industries/zed#43207) is closed as not planned. |
| Web | **Community only** | WASM backends exist in forks (WebGPU with WebGL2 fallback). Reports of builds needing non-default features to compile on stable Rust (the default `multithreaded` feature pulls a threads dependency). No official upstream web target. |

Consequences:

1. Android and web depend on forks that track upstream GPUI at their own pace. We must **pin exact git revisions**,
   expect to carry patches ourselves, and budget for maintaining (or contributing to) those backends.
2. **Phase 0 is a go/no-go spike** on exactly those two platforms before any real porting happens. If either fails,
   the fallback options are listed under "If the spike fails".
3. GPUI's own API is not stable (pre-1.0, driven by Zed's needs). Pin a revision and upgrade deliberately, not
   continuously.

## Target architecture

A Cargo workspace next to the existing pnpm workspace, so both coexist during the migration:

```
Cargo.toml                 # [workspace]
crates/
  beatseam-api/            # wire types (serde), API_VERSION, ApiClient (HTTP via a trait)
  beatseam-core/           # all platform-independent logic, no GPUI dependency
  beatseam-audio/          # decoding (symphonia) + our own mixer/crossfade engine
  beatseam-platform/       # traits for every platform seam + shared helpers
  beatseam-ui/             # GPUI views/components, theme, icons, navigation
  beatseam-app/            # the binary/cdylib: wires platform impls into core + ui
  platform-desktop/        # Windows/Linux/macOS impls (std::fs, rusqlite, SMTC/MPRIS, keyring, Discord IPC pipe)
  platform-android/        # JNI glue; reuses the existing Kotlin modules where they make sense
  platform-web/            # wasm-bindgen/web-sys impls (FS Access API, OPFS, IndexedDB/SQLite-wasm, Media Session)
apps/android-shell/        # minimal Gradle project: Activity hosting the GPUI surface + foreground service + Kotlin modules
apps/web-shell/            # index.html, service worker/PWA manifest, wasm build script
```

### Layering rules

* **`beatseam-core` must not depend on GPUI**, on any platform crate or on a specific async runtime. It is pure logic
  plus traits, so it is testable with plain `cargo test`, reusable by a future Rust server, and immune to GPUI churn.
  Async work goes through a small `Spawner`/`Clock` trait the app supplies (GPUI's executors natively,
  `wasm-bindgen-futures` on web), because on wasm we cannot assume threads.
* **`beatseam-ui` is the only crate that depends on GPUI.** It renders core state and dispatches actions; it holds no
  business logic. This replaces both `packages/ui` and the two large inline `App.tsx` files, so the "keep
  `apps/mobile/App.tsx` and `apps/web/src/App.tsx` in sync" problem disappears by construction: there is one app root.
* Platform crates implement `beatseam-platform` traits and nothing else. `#[cfg(target_os = ...)]`/`#[cfg(target_arch
  = "wasm32")]` belongs in those crates and in `beatseam-app`, not sprinkled through core or UI.

### State management (replacing Redux)

The Redux store (`packages/core/src/store/*`: library, navigation, notifications, persistence, playback, restore,
settings) becomes a set of plain Rust state structs with explicit action enums and pure `reduce` functions in
`beatseam-core`. That keeps the current reducer tests portable almost one-to-one. In the UI, each slice is wrapped in a
GPUI `Entity`, and views subscribe to the entities they read (GPUI's `cx.notify()`/`observe` replaces `useSelector`).
Thunks become async functions in core that take the platform services and an action sink.

We deliberately do not let GPUI's `Entity` types leak into core; the adapter between "core state + actions" and
"GPUI entities" lives in `beatseam-ui`.

### Audio: own the mixer

Today there are three engines behind `AudioEngine`: Web Audio on web, `react-native-audio-api` on Android (with a local
patch for a seek memory leak), and a C++/WinRT engine on Windows whose `decodeFile` resolves before PCM is available.
The crossfade is scheduled as gain automation on each engine's nodes, and much of `types.ts` documents per-engine
workarounds (`awaitAnalysisReady`, `prepareBuffer`, native `analyzeTrack`).

In Rust we replace this with **one engine we own**:

* **Decoding**: `symphonia` (MP3, FLAC, AAC/MP4, Ogg Vorbis, WAV; pure Rust, builds for wasm). Streaming decode
  instead of "decode the whole file up front", which also addresses the press-to-first-sound TODO.
* **Output**: `cpal` (WASAPI on Windows, AAudio/Oboe on Android, ALSA/PipeWire on Linux, CoreAudio on macOS, Web Audio
  via an `AudioWorklet` on web). The output callback pulls from **our mixer**, which mixes N sources with
  sample-accurate per-source gain curves. Equal-power crossfade, normalization gain and future rate changes become
  plain arithmetic in our code instead of engine automation APIs.
* **Analysis** (BPM, loudness, silence) runs on the decoded PCM in Rust on a background thread natively (a web worker
  on wasm, or chunked on the main thread as a first step). The "Windows analysis stutters the UI thread" workaround
  disappears because no platform runs analysis on the UI thread any more.
* Because we own the sample stream, the crossfade rework TODO (continuous BPM calibration on "the past few seconds",
  running both tracks in parallel with the next one muted, a live waveform/beat debug view) becomes much simpler: the
  mixer can tap recent PCM into a ring buffer that analysis and the debug view read.

The existing `AudioEngine` interface is a good contract to keep conceptually (`createSource`, `scheduleStart`,
gain curves, `now()`), so `PlaylistPlayer`/`TrackPlayer`/preload scheduling port without being redesigned.

### Platform seams

| Seam | Desktop (Windows/Linux/macOS) | Android | Web |
| --- | --- | --- | --- |
| File access (`FileAccess`) | `std::fs` + native folder picker (`rfd`) | SAF via JNI, reusing `BeatseamFileAccessModule.kt` logic | File System Access API via `web-sys`; server-backed access unchanged |
| Library store (`LibraryStore`) | `rusqlite` (bundled) | `rusqlite` (bundled) | SQLite-wasm on OPFS, or IndexedDB via `idb` behind the same trait (decide in Phase 0) |
| Offline downloads backend | files on disk | files on disk (existing native streaming+hashing logic ported to Rust, `sha2`) | OPFS (port of `apps/web/src/adapters/offlineBackend.ts`) |
| HTTP (`ApiClient`, Last.fm, update check) | `reqwest` (rustls) | `reqwest` (rustls) | `reqwest`'s wasm backend (`fetch`) |
| Secret storage (device token) | Windows Credential Manager / Secret Service / Keychain via `keyring` | Android Keystore via JNI (`BeatseamSecretStoreModule.kt`) | cookie session (unchanged) |
| OS media controls | `souvlaki` (SMTC on Windows, MPRIS, macOS Now Playing) | `MediaSession` + foreground service in Kotlin, driven over JNI | Media Session API via `web-sys` |
| Discord rich presence | named pipe / Unix socket IPC (`discord-ipc-0`), protocol from `richPresence.ts` ported to core | existing Kotlin binder module over JNI | not supported (as today) |
| Background execution | n/a | foreground service keeps the process alive; Rust timers are not paused like RN's JS timers | n/a |
| Cover art resizing | `image` crate | `image` crate | `image` crate (wasm) |
| Icons | MDI SVG paths rendered by GPUI's SVG support | same | same |
| App update check | GitHub releases (core) + open URL | existing `AppUpdateModule.kt` via JNI | n/a (PWA update) |

On Android the shell stays a small Kotlin project: an `Activity` hosting the GPUI surface (as `gpui-mobile` does), the
foreground playback service, the `MediaSession`, and the Kotlin modules we keep. Everything else moves into the Rust
`cdylib` loaded by that activity.

### Server compatibility and the wire format

* `beatseam-api` holds serde types mirroring `packages/api/src/schemas.ts`, the same `API_VERSION`, the
  `X-Beatseam-Api` request header and the `ApiVersionError` check against `X-Beatseam-Api-Versions`.
* Response types follow the existing rules: unknown fields are ignored (no `deny_unknown_fields`), and fields added
  after the first release are `Option<T>` with a fallback, exactly like `.optional()` today.
* **Contract tests keep the two sides honest** while both exist: a script exports JSON Schema from the zod schemas
  (`z.toJSONSchema`) plus a set of recorded request/response fixtures from the server's tests, and `beatseam-api`'s
  tests deserialize every fixture and round-trip every request type. CI runs both, so a server change that the Rust
  client can't read fails the build. This is the place where drift would otherwise silently creep in.

## What we expect to gain

Concrete things in `CLAUDE.md` that the new architecture closes or makes simpler:

* Windows icon codepoint map (`Icon.windows.tsx`) goes away: GPUI renders the MDI SVG paths directly on every platform.
* Windows "The file is in use" concurrent WinRT read stalls: desktop file access is plain `std::fs`.
* Windows tonearm rotation bug in `CrossfadeArt`: one renderer, one transform implementation.
* Windows SMTC, Discord RPC, credential storage and `writeFileText` placeholders: all have off-the-shelf Rust crates.
* Windows `useWindowDimensions()` not updating on resize (the RNW Fabric bug): GPUI gets window resizes natively, so the
  wide tier of the responsive layout becomes testable on desktop.
* Android backgrounding bugs caused by paused JS timers and the `whatwg-fetch` `setTimeout` settle (`backgroundSafeFetch.ts`,
  `BackgroundTicks`, the paused `ApiClient` timeout): Rust threads and timers are not paused with the activity.
* `react-native-audio-api` seek memory leak patch: no longer a dependency.
* Hermes blocking F-Droid: no Hermes, no prebuilt JS engine. (F-Droid still needs a reproducible Rust + Gradle build;
  that is a known, solvable path.)
* "Metadata sync is slow and the app is unresponsive while it runs": metadata/analysis runs off the UI thread natively.
  (It must still be chunked on wasm if we don't use workers.)

What we lose or must re-earn:

* **Accessibility.** RN maps to native accessibility trees on Android; GPUI's accessibility support is limited and
  the mobile fork lists it as incomplete. This is a real regression risk and is part of the Phase 0 evaluation.
* **Text input / IME on Android** (search boxes, lyrics editor, playlist names) is listed as incomplete in the mobile
  fork. The lyrics editor depends on it.
* Native look and platform conventions (scroll physics, long-press, haptics, back gesture) are ours to implement.
* Web bundle size and first load: a GPUI wasm app is several MB. Needs measuring; PWA caching softens it.
* Fast iteration: Metro/Vite hot reload is replaced by Rust rebuilds. Desktop iteration is fine; Android/web rebuild
  loops are slower and need good scripts.
* jsmediatags is replaced by `lofty` (or symphonia's metadata) for tags, cover art and embedded lyrics (`USLT`; `SYLT`
  and FLAC/MP4/Ogg lyrics, which are TODOs today, come almost for free with `lofty`).

## Phases

### Phase 0: spike and go/no-go (Android + web first, deliberately)

Desktop is the low-risk platform, so the spike targets the two that might not work at all. Build one throwaway app
that does, on **Android (arm64 phone, the existing test device)** and **web (Chromium + Firefox)**:

1. A GPUI window with a scrolling list of ~5k rows, a text input with IME (test a non-Latin keyboard), a custom-drawn
   rotating record + tonearm, and a slider. Check frame rate and touch handling (scroll fling, long press).
2. `cpal` + `symphonia` playing two local files with an equal-power crossfade from our own mixer, while the app is
   backgrounded on Android (foreground service) and while the tab is in the background on web.
3. Reading a file through SAF (Android) and through the File System Access API (web).
4. Measure: cold start time, press-to-first-sound, APK size, wasm size, memory.
5. Check what the platform accessibility tree exposes (TalkBack on Android, screen reader on web).

Exit criteria: all of the above work, or have a credible fix path we are willing to own. Decide the web storage
option (SQLite-wasm vs IndexedDB) and pin the GPUI and backend revisions. Record the outcome in this document.

#### If the spike fails

* **Web fails, Android works**: keep the current react-native-web build for the web (it keeps working against the
  same server), or compile `beatseam-core` to wasm and keep a thin web UI on top of it, so logic is still shared.
* **Android fails**: same idea in reverse: share `beatseam-core` + `beatseam-audio` through JNI/UniFFI under the
  existing RN UI (or a Kotlin UI), and use GPUI on desktop only.
* **Both fail**: GPUI for desktop only; evaluate a UI toolkit with first-party Android + web support for the rest.
  The core/audio crates are useful under any of these outcomes, which is why Phase 1 does not depend on GPUI.

### Phase 1: `beatseam-core`, `beatseam-api`, `beatseam-audio` (no UI)

Port in dependency order, porting the matching Vitest tests along with each module (the tests are the spec):

1. Pure logic: `playlist` (m3u8, reorder), `lyrics` (LRC parse/format, matching, translation merge), `metadata`
   helpers (durations, titles, content hash), `crossfade` (transition plan, equal-power gain, visualization),
   `analysis` (BPM, loudness, silence), `scrobble` (queue, tracker, md5/Last.fm signing), `discord` protocol,
   `tasks` (task queue), `appUpdate`.
2. `beatseam-api` types + `ApiClient` + contract tests against the zod schemas/fixtures.
3. `library-scan`, `library-store` traits, `offline` (`OfflineLibrary`), `server-mode` (sync, mutation queue, device
   playback, lyrics editing, covers).
4. `playback` (`TrackPlayer`, `PlaylistPlayer`, preload scheduler, shuffle) against `beatseam-audio`'s engine trait,
   with a fake engine for tests like the current ones.
5. State + actions (port of the store slices and thunks, including the latency-sensitive `restorePlayback` path and its
   rule: only the last-played root/playlist and the current track's lyrics block startup).

Deliverable: a small CLI (`beatseam-cli`) that scans a folder, plays a playlist with crossfades, and syncs against a
real server. Useful for debugging forever after, and proof that core works without a UI.

### Phase 2: desktop app (Windows first, Linux in CI)

Build `beatseam-ui` and `platform-desktop`: library, playlist/track lists, now playing with `CrossfadeArt` and synced
lyrics, settings, server sign-in (device code flow), offline downloads, lyrics picker/editor/sync screen, onboarding,
notifications center, the 3-tier responsive layout. Ship it as the Windows build once it reaches parity with the RNW
app (which is the weakest RN platform today, so the bar is lowest and the gain is highest). MSIX packaging for the
Store job in `native-builds.yml`.

### Phase 3: Android

`platform-android` + `apps/android-shell`. Port or keep the Kotlin modules behind JNI. Install side by side with the
release build (separate application id suffix, as the debug build does today) until parity, then switch the release
pipeline. Verify live on the test phone: backgrounded playback, media notification, Discord, Last.fm, offline
downloads, lyrics sync gestures.

### Phase 4: web

`platform-web` + `apps/web-shell`. The server keeps serving the web app from the same origin (cookie session,
`connectSameOrigin`), so the Docker image switches from the Vite build output to the wasm bundle. Browser-picked local
folders keep using the File System Access API (Chromium only, as today).

### Phase 5: retire the TypeScript clients

Remove `apps/mobile`, `apps/web`, `packages/ui`, and the client-only parts of `packages/core` once each platform has
shipped from Rust for at least one release. `packages/api` and whatever `packages/core` code the server imports stay
until the server question is decided. Update `CLAUDE.md`'s verification section (`cargo fmt --check`,
`cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, per-target builds) and its conventions.

### Later: the server

Once `beatseam-core` exists, the server's scanner and analysis (`apps/server/src/scanner`, which currently decodes via
ffmpeg and runs the TS analysis in worker threads) could use the same Rust code, either as a Node native addon
(`napi-rs`) or by porting the server to Rust (e.g. `axum` + `sqlx` on the same Postgres schema). Out of scope until
the clients are done; listed here so the core crate is designed not to preclude it.

## Tooling and CI

* `rust-toolchain.toml` pinning a stable toolchain; targets `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`,
  `aarch64-linux-android`, `wasm32-unknown-unknown`.
* CI adds a Rust job: `cargo fmt --check`, `cargo clippy`, `cargo test --workspace` (core/api/audio run headless),
  plus build-only jobs for Android (`cargo ndk`) and wasm (`wasm-bindgen`/`trunk`) once those phases start.
* The existing pnpm jobs keep running unchanged until Phase 5.
* GPUI and the community backends are pinned by git revision in `Cargo.toml`; upgrades are their own PRs.

## Open questions for the maintainer

1. Is dropping react-native-web acceptable if the GPUI web backend turns out to be workable but rough (bundle size,
   accessibility), or is web parity a hard requirement before switching?
2. How much fork maintenance on GPUI's Android/web backends are we willing to take on?
3. Should Windows ship first (lowest bar, biggest win), or should Android lead since it is the most-used client?
4. Is a Rust server on the table at all, or should the core crate only ever target clients?
5. Accessibility: what is the minimum acceptable level for a release (TalkBack-navigable transport and lists)?
