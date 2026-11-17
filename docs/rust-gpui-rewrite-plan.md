# Rewriting Beatseam in Rust

Status: **proposal / plan**. Nothing in this document is implemented yet. It covers the client apps and the server. The
phases below are ordered so that each one ends in something shippable or in a clear go/no-go decision, and the existing
React Native apps keep shipping until each platform's Rust app reaches parity.

## Goals

* One Rust codebase for the client apps on **Android, Windows and web**. Desktop Linux and macOS come along almost for
  free, since they are GPUI's best-supported targets.
* Share as much code as possible: playback, crossfade, analysis, library/playlist/lyrics logic, server-mode sync,
  scrobbling, and the view models (what each screen shows and which actions it offers). The rendering layer is shared
  where GPUI qualifies (see "Decisions"); everywhere else only rendering is per platform, never logic. Per-platform
  code should otherwise be limited to thin adapters (file access, OS media controls, secret storage, background
  execution), the same way `apps/mobile/src/adapters/*` works today but smaller.
* **Rewrite the server (`apps/server`) in Rust too.** This is a hard requirement for the web: the web app is served by
  the server, and the server's scanner, analysis and lyrics matching should be the same `beatseam-core` code the
  apps run, not a second TypeScript copy.
* The **web UI** is a DOM UI in Rust (Leptos) rather than GPUI, held to the other platforms' look by automated
  **visual parity tests** (see "Visual parity testing").
* **No accessibility regression**: a Rust client replaces the current one on a platform only once screen readers can
  do everything there that they can do in the current app.
* Fix structural problems the current stack works around rather than solves (see "What we expect to gain").
* Keep the wire format compatible in both directions throughout (`docs/api-compatibility.md` still applies). A Rust
  client is just another app release, and the Rust server is just another server release: old TS apps must keep
  working against it, and Rust apps against the old TS server.

## Non-goals (for now)

* iOS. Not supported today. The architecture below would not prevent it (the community mobile backend targets iOS too),
  but it is not planned.
* New features. Parity first. The open TODOs in `CLAUDE.md` move over as TODOs, except where the new architecture
  closes them as a side effect (listed below).

## Decisions

Settled with the maintainer (November 2026):

| Question | Decision | Consequence for the plan |
| --- | --- | --- |
| Server language | **Rust**, same routes, wire format and Postgres schema | `beatseam-server`, Phase 2a |
| GPUI fork maintenance | **Minimal: consume the community backends as-is.** Pin revisions, no local patches; when a backend lacks something we need, take the fallback instead of fixing it ourselves. | Android may not use GPUI (see below); upstream GPUI is used where it is officially supported |
| First client to ship | **Windows** (GPUI's official platform, and the weakest RN app today) | Phase 2b is Windows; Android and web follow |
| Accessibility bar | **Parity with today**: whatever TalkBack/Narrator/browser screen readers can do in the current app must still work. A release blocker per platform. | Rules out canvas rendering on web; makes Android GPUI conditional; must be verified for GPUI on Windows too |
| Web UI | **DOM-based, Leptos**, over the same wasm core. GPUI on wasm is not pursued (its canvas is invisible to screen readers). | `beatseam-web-ui` crate; design tokens exported as CSS custom properties |
| Android UI if GPUI doesn't qualify | **Jetpack Compose + UniFFI**: native Kotlin UI with full TalkBack/IME support, Rust core and audio through UniFFI bindings | Decided by the Phase 0 spike; Compose is the likely outcome given the two rows above |
| Visual parity strictness | **Tiered**: strict on playback-facing screens (now playing, crossfade art, track lists, mini player), looser on forms (settings, sign-in, dialogs) | Two threshold tiers in the parity suite |
| Legacy `/api/roots` + `/library` routes | **Dropped**, not ported. Only the current TS web app's composite file access calls them, and it already treats their absence as "no server roots". `fileAccess.server.ts`/`fileAccess.composite.ts` are removed from the TS web app before the Rust server ships, so nothing calls them. | Smaller server port |

## Where we are today

| Area | Today | Size |
| --- | --- | --- |
| `packages/core` | Playback (`PlaylistPlayer`, `TrackPlayer`, preload scheduler), crossfade planning, BPM/loudness/silence analysis, library scan, m3u8, lyrics matching/LRC, metadata (jsmediatags), offline library, scrobbling, Discord presence protocol, server mode (sync, mutation queue), Redux store | ~21k lines TS, 59 test files (~9.6k lines) |
| `packages/ui` | Shared React Native components and hooks (library, now playing, lyrics editor/sync, settings, server, onboarding) | ~13k lines |
| `packages/api` | zod schemas for the `/api/v1` wire format, `API_VERSION` | ~1.2k lines |
| `apps/mobile` | RN app for Android + react-native-windows; adapters per platform; Kotlin native modules (SAF file access, offline downloads, keystore, background ticks, Discord binder RPC, app update); C++/WinRT modules on Windows (audio engine, file access, cover resizer) | `App.tsx` ~1.3k lines + ~2.3k adapters + native |
| `apps/web` | Vite + react-native-web; Web Audio engine, File System Access API, IndexedDB, OPFS offline backend, Media Session | `App.tsx` ~1.3k lines + ~2.7k adapters |
| `apps/server` | NestJS 11 + Prisma 7 on Postgres (27 models, 11 migrations); auth (argon2 passwords, passkeys/WebAuthn, TOTP, recovery codes, sessions + CSRF, device-code grants, invites); library scan + job queue; ffmpeg decode + analysis in worker threads; sync change feed, mutations, user playlists/settings/playback state; lyrics serving/editing/reports; Last.fm proxy (session sealed with AES-256-GCM); account pages (`public/`); legacy `/api/roots` + `/library` routes; sharp for covers | ~4.7k lines + 20 Vitest suites, mostly black-box over HTTP |

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

1. **Web does not use GPUI** (see "Decisions"): accessibility parity rules out a canvas-rendered UI.
2. **Android uses GPUI only if the community backend, used as-is, meets the accessibility and IME bar.** Otherwise
   Android gets a Jetpack Compose UI over the shared Rust crates. Phase 0 decides this before any Android UI work.
3. **Windows accessibility must be verified too.** GPUI has AccessKit-based plumbing (UI Automation on Windows), but
   it is unconfirmed how much of it upstream GPUI exposes today; one fork notes Zed itself sets no accessibility
   properties on its elements. Phase 0 checks Narrator on a GPUI list, buttons and a slider.
4. GPUI's own API is not stable (pre-1.0, driven by Zed's needs). Pin a revision and upgrade deliberately, not
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
  beatseam-viewmodel/      # per-screen view models + actions, design tokens, fixture rendering hooks (no UI toolkit)
  beatseam-ui/             # GPUI views/components (desktop, and Android if it qualifies)
  beatseam-web-ui/         # Leptos views/components (web)
  beatseam-ffi/            # UniFFI bindings to core/audio/viewmodel for the Compose UI (only if Android isn't GPUI)
  beatseam-app/            # the desktop binary: wires platform impls into core + ui
  platform-desktop/        # Windows/Linux/macOS impls (std::fs, rusqlite, SMTC/MPRIS, keyring, Discord IPC pipe)
  platform-android/        # JNI glue; reuses the existing Kotlin modules where they make sense
  platform-web/            # wasm-bindgen/web-sys impls (FS Access API, OPFS, IndexedDB/SQLite-wasm, Media Session)
  beatseam-server/         # axum server binary (replaces apps/server)
  beatseam-fixtures/       # deterministic app states + seeded libraries for tests and visual parity (see below)
apps/android/              # Gradle project: Compose UI (or an Activity hosting the GPUI surface) + foreground service + Kotlin modules
apps/web-shell/            # index.html, service worker/PWA manifest, Leptos/wasm build (trunk)
```

### Layering rules

* **`beatseam-core` must not depend on GPUI**, on any platform crate or on a specific async runtime. It is pure logic
  plus traits, so it is testable with plain `cargo test`, shared with `beatseam-server` (which runs it on tokio), and
  immune to GPUI churn.
  Async work goes through a small `Spawner`/`Clock` trait the app supplies (GPUI's executors natively,
  `wasm-bindgen-futures` on web), because on wasm we cannot assume threads.
* **`beatseam-viewmodel` holds everything a screen needs except drawing it**: derived display data (formatted
  titles, durations, "Playing now on <device>", lyrics line states, crossfade art angles/spin speeds), the actions
  each control dispatches, navigation, and the design tokens. Every UI (GPUI, Leptos, Compose via UniFFI) is a thin
  renderer of these view models, holding no business logic. This is what keeps three renderers from drifting the way
  `apps/mobile/App.tsx` and `apps/web/src/App.tsx` drift today: the shape of each screen is defined once.
* **`beatseam-ui` is the only crate that depends on GPUI**, `beatseam-web-ui` the only one that depends on Leptos.
* Platform crates implement `beatseam-platform` traits and nothing else. `#[cfg(target_os = ...)]`/`#[cfg(target_arch
  = "wasm32")]` belongs in those crates and in `beatseam-app`, not sprinkled through core or UI.

### State management (replacing Redux)

The Redux store (`packages/core/src/store/*`: library, navigation, notifications, persistence, playback, restore,
settings) becomes a set of plain Rust state structs with explicit action enums and pure `reduce` functions in
`beatseam-core`. That keeps the current reducer tests portable almost one-to-one. In the UI, each slice is wrapped in a
GPUI `Entity`, and views subscribe to the entities they read (GPUI's `cx.notify()`/`observe` replaces `useSelector`).
Thunks become async functions in core that take the platform services and an action sink.

We deliberately do not let GPUI's `Entity` types leak into core or the view models; the adapter between "core state
+ actions" and "GPUI entities" lives in `beatseam-ui`. On web the same state is exposed as Leptos signals in
`beatseam-web-ui`, and for Compose as observable `StateFlow`s through UniFFI callbacks.

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
| File access (`FileAccess`) | `std::fs` + native folder picker (`rfd`) | `std::fs` under `MANAGE_EXTERNAL_STORAGE`, as the current app already does (no SAF; verified in Phase 0 item 4) | File System Access API via `web-sys`; server-backed access unchanged |
| Library store (`LibraryStore`) | `rusqlite` (bundled) | `rusqlite` (bundled) | SQLite-wasm on OPFS, or IndexedDB via `idb` behind the same trait (decide in Phase 0) |
| Offline downloads backend | files on disk | files on disk (existing native streaming+hashing logic ported to Rust, `sha2`) | OPFS (port of `apps/web/src/adapters/offlineBackend.ts`) |
| HTTP (`ApiClient`, Last.fm, update check) | `reqwest` (rustls) | `reqwest` (rustls) | `reqwest`'s wasm backend (`fetch`) |
| Secret storage (device token) | Windows Credential Manager / Secret Service / Keychain via `keyring` | Android Keystore via JNI (`BeatseamSecretStoreModule.kt`) | cookie session (unchanged) |
| OS media controls | `souvlaki` (SMTC on Windows, MPRIS, macOS Now Playing) | `MediaSession` + foreground service in Kotlin, driven over JNI | Media Session API via `web-sys` |
| Discord rich presence | named pipe / Unix socket IPC (`discord-ipc-0`), protocol from `richPresence.ts` ported to core | existing Kotlin binder module over JNI | not supported (as today) |
| Background execution | n/a | foreground service keeps the process alive; Rust timers are not paused like RN's JS timers | n/a |
| Cover art resizing | `image` crate | `image` crate | `image` crate (wasm) |
| Icons | MDI SVG paths rendered by GPUI's SVG support | same (GPUI), or `ImageVector`s generated from the same paths (Compose) | inline SVG from the same paths |
| App update check | GitHub releases (core) + open URL | existing `AppUpdateModule.kt` via JNI | n/a (PWA update) |

On Android the Kotlin project holds the UI (Compose, or an `Activity` hosting the GPUI surface if GPUI qualifies),
the foreground playback service, the `MediaSession`, and the Kotlin modules we keep. All logic, audio and the view
models live in the Rust `cdylib`; with Compose, Kotlin talks to it only through the generated UniFFI bindings.

### The wire format

* `beatseam-api` holds serde types mirroring `packages/api/src/schemas.ts`, the same `API_VERSION`, the
  `X-Beatseam-Api` request header and the `ApiVersionError` check against `X-Beatseam-Api-Versions`. Both the Rust
  apps and `beatseam-server` use these types, so after the migration there is exactly one definition of the wire
  format again.
* Response types follow the existing rules: unknown fields are ignored (no `deny_unknown_fields`), and fields added
  after the first release are `Option<T>` with a fallback, exactly like `.optional()` today.
* **Contract tests keep the TS and Rust definitions honest** while both exist: a script exports JSON Schema from the
  zod schemas (`z.toJSONSchema`) plus request/response fixtures recorded from the server's API tests, and
  `beatseam-api`'s tests deserialize every fixture and round-trip every request type. A change on either side that
  the other can't read fails CI.

### The server in Rust

`beatseam-server` replaces `apps/server` with the same routes, the same wire format and **the same Postgres schema**,
so an existing deployment upgrades by swapping the Docker image: no data migration, and a rollback to the TS image
stays possible during the transition.

| Concern | TS today | Rust |
| --- | --- | --- |
| HTTP | NestJS + Express | `axum` + `tower-http` (compression, static files, request limits) |
| Database | Prisma 7 (`schema.prisma`, 11 migrations) | `sqlx` (Postgres, compile-time checked queries). The existing migrations become the baseline; `sqlx` takes over from a migration that is a no-op on an up-to-date database, and Prisma's `_prisma_migrations` table is left alone so a rollback still works. |
| Passwords | `@node-rs/argon2` | `argon2` crate. Both store PHC strings, so existing hashes verify unchanged (covered by a test with hashes produced by the TS server). |
| Tokens, TOTP, recovery codes | `node:crypto` | `sha2`, `hmac`, `rand`, `subtle`: same formats (base64url tokens, SHA-256 hex at rest) |
| Secret box (Last.fm session at rest) | AES-256-GCM, key via HKDF-SHA256 (`beatseam-secret-box` salt) | `aes-gcm` + `hkdf`, byte-compatible; tested against values sealed by the TS server |
| Passkeys | `@simplewebauthn/server` | `webauthn-rs`. Stored credentials must stay usable: verify the stored public key/counter format against real registrations made by the TS server before switching. |
| Throttling | `@nestjs/throttler` + `failureLimiter` | `tower_governor` + a port of the failure limiter |
| Scheduling, job queue | `@nestjs/schedule`, `jobQueue.ts`, worker threads | tokio tasks + a bounded worker pool (`spawn_blocking`/rayon) for decode/analysis |
| Decode + analysis | ffmpeg subprocess + TS analysis | `beatseam-audio` (symphonia) + `beatseam-core` analysis: the same code the apps run. ffmpeg stays as a fallback for formats symphonia can't read, so nothing that scans today stops scanning. |
| Tags, embedded lyrics | jsmediatags | `lofty` (shared with the apps) |
| Covers | sharp | `image` crate (shared with the apps) |
| Account pages | `public/account.*` + `pages.controller.ts` | served as-is at first, then templated with `askama` if needed |
| Config | `@nestjs/config` env vars | same env var names (`BEATSEAM_*`, `DATABASE_URL`), so compose files keep working |
| Web app hosting | serves the Vite build | serves the new web build |

The legacy `/api/roots` + `/library` routes are ported only if something still uses them when the server switches
(see the server-mode TODO in `CLAUDE.md`); otherwise this is the moment to drop them.

**Conformance testing.** The existing server API suites (`apps/server/test/*.api.test.ts`) already talk to the server
over real HTTP through `TestClient(baseUrl)`. They become the conformance suite: CI runs the same suites against both
the TS server and `beatseam-server`, and the Rust server is ready to replace the TS one when all of them pass. The few
tests that reach into the Nest app in-process (the injectable TOTP clock, the scan stack) need a test-only hook in the
Rust server (e.g. a clock override endpoint compiled only with a `test-hooks` feature). Old-app compatibility is
covered the same way: the current TS `ApiClient` tests (`apiClient.test.ts`) run against the Rust server too.

### The web UI

The web client runs the shared Rust crates (`beatseam-core`, `beatseam-api`, `beatseam-audio`,
`beatseam-viewmodel`) compiled to wasm, with a **Leptos** UI (`beatseam-web-ui`) rendering real DOM. That gives screen
readers, browser text input/IME, text selection, find-in-page and native scrolling for free, and Leptos's
fine-grained signals map directly onto the view models. The design tokens are generated as CSS custom properties, and
icons are inline SVG from the same MDI paths. The visual parity tests below hold it to the GPUI look.

### Visual parity testing

To make "the web UI may differ in implementation but must not drift in appearance" enforceable:

* **Shared design tokens.** Colors (all four themes and the accent palette), spacing, radii, font sizes and icon
  sizes live in `beatseam-viewmodel`. GPUI reads them directly, Leptos gets them generated as CSS custom properties,
  Compose (if used) as a generated Kotlin theme object. Most drift is a token copied by hand, so this removes the
  most common cause.
* **Fixture states.** `beatseam-fixtures` defines a catalog of deterministic app states built from the core state
  types: empty library, onboarding, a library with a seeded playlist, a long track list, now playing with synced
  lyrics mid-line, the crossfade between two tracks at a fixed progress, settings in each theme, server sign-in,
  lyrics sync screen, the notification center, and each one at the three layout tiers (phone, medium, wide). Time,
  random ids, cover art and fonts are fixed, and animations are frozen at a given timestamp, so a state renders
  identically every run.
* **Per-platform baselines.** Each UI renders every fixture state to PNG in CI: GPUI headlessly on Linux (offscreen
  rendering; confirming GPUI can do this in CI is part of Phase 0), and the Leptos build in Playwright's Chromium
  (already installed in CI images) at the same viewport sizes. Each is compared with approved baselines checked into
  the repo; an intended change updates the baseline in the same PR, so reviewers see the before/after images.
* **Cross-platform parity check.** The web screenshots are additionally compared with the GPUI desktop screenshots of
  the same fixture state with a perceptual metric (SSIM or a per-region tolerance) rather than exact pixels, since
  text rasterization and antialiasing will never match exactly between renderers. A drop below the threshold fails
  CI with a diff image. Two threshold tiers: **strict** for playback-facing states (now playing, crossfade art, track
  lists, mini player, lyrics) and **loose** for forms (settings, sign-in, dialogs, onboarding), where native and web
  controls may reasonably differ. Each fixture declares its tier; the thresholds themselves are tuned once the first
  pair of baselines exists.
* **Structural checks** catch what images miss: each fixture also emits the list of visible text strings and
  interactive elements (labels, roles, their order), taken from the platform accessibility tree (UI Automation for
  GPUI on Windows, the DOM accessibility tree via Playwright, Compose's semantics tree via its test APIs), and those
  must be identical across platforms. This doubles as the **accessibility regression test**: a control that loses its
  label or role on one platform fails the check.
* Android gets the same fixtures captured on an emulator in a nightly job (not per PR, since it is slow), compared
  with the GPUI desktop baselines (Compose screenshot tests make this cheap if Android ends up on Compose).

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
* One implementation of scanning, tag reading, analysis and lyrics matching for the server and every app, instead of
  the server shelling out to ffmpeg while the apps analyse in TS. A track analysed on the server and on a phone gets
  the same loudness and BPM by construction.
* The server image no longer needs Node, and in most libraries not ffmpeg either (kept only as a fallback decoder).

What we lose or must re-earn:

* **One shared renderer.** Today `packages/ui` renders on every platform. With web on Leptos (and likely Android on
  Compose), rendering is written two or three times. The shared view models, design tokens and the visual parity
  suite are what make that acceptable; they are not optional.
* **Accessibility on GPUI** has to be verified on Windows (and Android, if GPUI is used there) rather than assumed.
* On the GPUI side, native platform conventions (scroll physics, long-press, keyboard focus) are ours to implement.
* Web bundle size: the wasm core (symphonia, lofty, analysis) is likely 1-3 MB compressed. Needs measuring; PWA
  caching softens it.
* Fast iteration: Metro/Vite hot reload is replaced by Rust rebuilds. Desktop iteration is fine; Android/web rebuild
  loops are slower and need good scripts.
* jsmediatags is replaced by `lofty` (or symphonia's metadata) for tags, cover art and embedded lyrics (`USLT`; `SYLT`
  and FLAC/MP4/Ogg lyrics, which are TODOs today, come almost for free with `lofty`).

## Phases

### Phase 0: spike and go/no-go

Build throwaway apps that answer the open technical questions before any real porting:

1. **GPUI on Windows** (must pass, Windows ships first): a scrolling list of ~5k rows, a text input, a custom-drawn
   rotating record + tonearm, a slider. Check that **Narrator** can reach, name and operate every one of them through
   UI Automation. If it can't, this is the one result that reopens the UI decision for desktop, so it comes first.
2. **GPUI on Android, community backend used as-is**: the same view on the test phone, checked against the bar:
   TalkBack can operate everything, IME works with a non-Latin keyboard, scroll fling and long press feel native.
   Anything short of that means **Android goes Compose + UniFFI**, with no attempt to patch the backend.
3. **Audio**: `cpal` + `symphonia` playing two local files with an equal-power crossfade from our own mixer, on
   Windows, on Android while backgrounded (foreground service), and on web in an `AudioWorklet` while the tab is in the
   background.
4. **File access**: `std::fs` under All files access (Android; the app does not use SAF) and the File System Access
   API (web) from Rust. Both done, see the results below.
5. **UniFFI round trip** (only if item 2 failed): a Compose screen observing a Rust view model, with actions flowing
   back, to confirm the binding shape before Phase 3.
6. **Visual parity pipeline**: render one view headlessly to PNG with GPUI on a Linux CI runner (no GPU, no display)
   and the same view from a Leptos build in Playwright, run the perceptual comparison on the pair, and extract both
   accessibility trees for the structural check.
7. Measure: cold start, press-to-first-sound, installer/APK size, wasm size, memory.

Exit: record the results here, pin the GPUI revision (and the Android backend's, if used), decide the web storage
option (SQLite-wasm vs IndexedDB), and mark Android as GPUI or Compose.


#### Phase 0 results so far

**Item 1, GPUI on Windows** (spike in `spikes/windows-gpui/`, GPUI pinned at `zed-industries/zed` rev `a701d985`, no
patches; full table in that folder's README):

* Upstream GPUI at this revision has an AccessKit-based accessibility API (roles, labels, numeric values, accessible
  actions), so the plan's doubt about whether it exists at all is settled: it does, and on Windows it surfaces through
  UI Automation. The spike's window exposed a labelled Edit, Slider (0..100, `RangeValue`), Image and List/ListItems.
* Gaps seen through UI Automation: the slider's `SetValue` and the text field's `ValuePattern.SetValue` are accepted
  but do nothing; the text field has no `TextPattern` (no caret/selection reading); the list has no
  `Scroll`/`Selection` patterns and only the visible rows exist as nodes. Under "no local patches" these could only be
  worked around in our own elements.
* **The Narrator pass was skipped by the maintainer**, so the accessibility bar is *not* verified on Windows. The
  release blocker in "Decisions" ("verified per platform") still stands and has to be met before the Windows app
  ships; it is just no longer a gate for continuing Phase 0.
* Release build numbers (spike only): 10.3 MB executable, ~0.7 s cold start to a visible window, ~2% of one core idle
  with the record spinning, 63 MB memory with the 5k-row list, flat while scrolling.

**Item 2, GPUI on Android** (spike in `spikes/android-gpui/`, backend `longbridge/gpui-mobile` at `7548ebc4b2`, no
patches; full notes in that folder's README):

* The backend runs on a real phone (Galaxy S24+, Android 17): Vulkan rendering, the 5k-row list scrolls, the soft
  keyboard opens from a focused field and typing works (Latin keyboard; a non-Latin IME was not tried).
* It is built on `gpui-pre` crates.io snapshots, a different GPUI from the Zed revision the Windows spike pins, so
  the Android and desktop UI code would have to track two GPUI versions.
* Its README says accessibility is unfinished, but the code does contain an AccessKit-to-Android bridge, so the plan's
  assumption that there is nothing there was wrong. Whether TalkBack can actually operate a GPUI screen was **not
  verified**: the maintainer skipped the TalkBack pass (`uiautomator` can't probe it; it needs a client that stays
  attached).
* Licensing needs a look before any dependency: the backend is "GPL-3.0-or-later OR AGPL-3.0-or-later OR Apache-2.0";
  only the Apache-2.0 option fits this MIT repo.
* **Consequence under the plan's own rule** ("anything short of the accessibility and IME bar means Android goes
  Compose + UniFFI"): the bar is unproven, so Android is **not qualified for GPUI** and defaults to Compose + UniFFI.
  A TalkBack pass (plus a non-Latin keyboard) would be needed to reopen that.

**Item 3, audio on Windows** (spike in `spikes/windows-audio/`; full table in that folder's README):

* `symphonia` 0.6.1 decode, `cpal` 0.18 output and our own equal-power mixer work together on Windows: the mixer's
  five unit tests pass, an offline render of two real tracks shows a smooth loudness curve through a 6 s fade, and
  real-time playback through WASAPI ran with no stream errors and a worst callback of 17 us (0.1% of its buffer).
* Two things for the real engine: two loud tracks summed mid-fade peaked at 1.27, so normalization or a limiter is
  required; and the spike only does f32 output, linear resampling and whole-clip decoding.
* Listened to by the maintainer: sounded fine. Not yet done on Android (backgrounded, foreground service) or web (`AudioWorklet`).

**Item 6, visual parity pipeline** (spikes `parity-gpui`, `parity-web`, `parity-compare`; full notes in
`spikes/parity-gpui/README.md`):

* GPUI **can** render a fixture headlessly to PNG with no GPU or display on Linux (software Vulkan), in ~0.13 s per
  render after a 1.4 s start, and the output is byte-identical across runs when text uses only bundled fonts. It
  **cannot on Windows** at this revision (`current_headless_renderer()` is `None`), so GPUI baselines are generated on
  Linux CI.
* A Leptos twin screenshotted in Chromium compared against it with SSIM scored 0.961 untuned and 0.981 once line
  heights were made whole-pixel tokens on both sides; the remaining diff is sub-pixel text placement. So the tiered
  thresholds in "Visual parity testing" look achievable; tune them on real screens.
* Line heights (like colours and spacing) must be explicit pixel design tokens: the renderers' defaults differ a lot.
* The structural check cannot read GPUI's accessibility tree headlessly (`debug_a11y_tree_json()` is `None` unless
  assistive technology is connected). The plan's structural check should compare each platform's real tree against
  the structure derived from `beatseam-viewmodel`, rather than comparing platform trees to each other headlessly.

**Item 4, file access, web half** (spike `web-fsa`; full notes in `spikes/web-fsa/README.md`):

* The web `FileAccess` seam works from Rust through `web-sys`: recursive listing with sizes (361 files in 123 ms),
  byte-range reads via `Blob.slice` without reading the whole file, non-ASCII text read/write, and a `NotFoundError`
  that maps to a not-found variant. A directory handle restored from IndexedDB after a reload works from Rust. The
  wasm is 125 KB before any app code.
* Verified against an OPFS handle (a picker can't be automated). Still unverified: the permission re-prompt a picked
  folder needs after a reload (`requestPermission()` needs a user gesture) and I/O speed on a real disk folder.
* `showDirectoryPicker` is not in stable `web-sys`, so the picker call stays as a few lines of JS in the web shell.

**Item 4, file access, Android half** (in `spikes/android-gpui`, `src/fs_probe.rs`): the current app does not use SAF;
it reads with `java.io.File` under `MANAGE_EXTERNAL_STORAGE`, because a Samsung SAF picker rejected folder grants. On
the test phone, native `std::fs` under that same permission is enough, with no JNI:

* Walking the real library (1,990 files, 8.5 GB) took 1.96 s, a 64 KiB mid-file read 7 ms, and a UTF-8 write, read
  back and delete worked. `Android/data` stays denied.
* Without the grant the app sees only a filtered view: a 776-file walk of the same folder took 19.5 s and writes fail.
  So the existing "grant All files access" onboarding step stays.
* Consequence: the Android `FileAccess` is the same `std::fs` code as desktop, and the SAF-via-JNI row in the platform
  seams table is replaced. Offline downloads on Android are plain files too. Still unverified: Play Store policy for
  `MANAGE_EXTERNAL_STORAGE` (the app is distributed outside it today).

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

6. `beatseam-viewmodel`: per-screen view models, actions and design tokens, tested without any UI toolkit, and
   `beatseam-fixtures` built on them.

Deliverable: a small CLI (`beatseam-cli`) that scans a folder, plays a playlist with crossfades, and syncs against a
real server. Useful for debugging forever after, and proof that core works without a UI.

### Phase 2a: the server (can run in parallel with Phase 2b)

1. Make the conformance suite target-agnostic: the server API tests run against a URL, with the in-process hooks
   (clock, scan stack) behind a small test-hook interface both servers implement. Run it against the TS server first
   so it is known-good.
2. `beatseam-server` on the existing schema: config, health, auth (passwords, sessions + CSRF, TOTP, recovery codes,
   invites, device-code grants, passkeys), then library/tracks/covers/audio streaming (including range requests),
   sync + mutations + user data, lyrics (serving, editing, reports), Last.fm, scanner + job queue, account pages,
   static web hosting.
3. Before the switch, remove `fileAccess.server.ts`/`fileAccess.composite.ts` from the TS web app (the legacy
   `/api/roots` + `/library` routes are not ported) and ship that, so no client calls them any more.
4. Compatibility tests specific to the switch: verify password hashes, sealed Last.fm sessions, device tokens and
   passkeys created by the TS server, and switch a copy of a real database back and forth between the two images.
5. Ship as the Docker image once the whole conformance suite passes. Keep the TS image buildable for one release as
   the rollback path.

### Phase 2b: desktop app (Windows first, Linux in CI)

Build `beatseam-ui` and `platform-desktop`: library, playlist/track lists, now playing with `CrossfadeArt` and synced
lyrics, settings, server sign-in (device code flow), offline downloads, lyrics picker/editor/sync screen, onboarding,
notifications center, the 3-tier responsive layout. The GPUI baseline screenshots and accessibility trees are built
here, alongside the views they cover. Ship it as the Windows build once it reaches feature parity with the RNW app
and passes the accessibility bar (Narrator). MSIX packaging for the Store job in `native-builds.yml`.

### Phase 3: Android

`platform-android` + `apps/android`, with a Compose UI over `beatseam-ffi` (or `beatseam-ui` on GPUI, if Phase 0
qualified it). Keep the Kotlin modules that are already Kotlin (MediaSession, Discord binder, Keystore, app update)
and call them directly from the Kotlin side; SAF file access and offline downloads move behind the Rust `FileAccess`
trait. Compose screenshot tests join the parity suite. Install side by side with the release build (separate
application id suffix, as the debug build does today) until parity, then switch the release pipeline. Verify live on
the test phone: backgrounded playback, media notification, Discord, Last.fm, offline downloads, lyrics sync gestures,
and a full TalkBack pass.

### Phase 4: web

`platform-web`, `beatseam-web-ui` (Leptos) and `apps/web-shell`. Depends on Phase 2a: the
Rust server serves the web app from the same origin (cookie session, `connectSameOrigin`), so the web build is
shipped inside the `beatseam-server` image. Browser-picked local folders keep using the File System Access API
(Chromium only, as today). The web UI ships only once the visual parity suite passes for every fixture state and a
screen reader pass (NVDA or VoiceOver) matches today's app.

### Phase 5: retire TypeScript

Remove `apps/mobile`, `apps/web`, `apps/server`, `packages/ui`, `packages/core` and `packages/api` once each platform
and the server have shipped from Rust for at least one release. The conformance suite and contract tests are then
ported to Rust (or kept as a small standalone TS test package, if that is less work) so they keep guarding old app
releases still in the wild. Update `CLAUDE.md`'s verification section (`cargo fmt --check`,
`cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, the visual parity suite, per-target builds) and
its conventions, and move `docs/api-compatibility.md`'s references from zod to the Rust types.

## Tooling and CI

* `rust-toolchain.toml` pinning a stable toolchain; targets `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`,
  `aarch64-linux-android`, `wasm32-unknown-unknown`.
* CI adds a Rust job: `cargo fmt --check`, `cargo clippy`, `cargo test --workspace` (core/api/audio run headless),
  plus build-only jobs for Android (`cargo ndk`, Gradle) and wasm (`trunk`) once those phases start.
* A server job runs the conformance suite against both servers (Postgres service container, as the current server
  tests already need).
* A visual parity job renders the fixture states for GPUI (headless) and Leptos (Playwright), runs the structural
  and accessibility-tree checks, and uploads diff images as artifacts on failure.
* The existing pnpm jobs keep running unchanged until Phase 5.
* GPUI (and the Android backend, if used) is pinned by git revision in `Cargo.toml`; upgrades are their own PRs. No
  local patches to it (see "Decisions").

## Open questions

All product decisions so far are recorded under "Decisions". What remains is technical and is answered by the
Phase 0 spike: whether GPUI meets the accessibility bar on Windows, whether Android uses GPUI or Compose, the web
storage backend, and whether GPUI can render headlessly in CI for the parity suite. If GPUI fails the Windows
accessibility check, the desktop UI choice comes back to the maintainer before Phase 2b starts.
