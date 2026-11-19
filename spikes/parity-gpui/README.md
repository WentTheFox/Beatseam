# Phase 0 spike: visual parity pipeline (item 6)

Throwaway for item 6 of Phase 0 in `docs/rust-gpui-rewrite-plan.md`: render the same fixed "now playing" fixture with
GPUI (headless) and with Leptos in a browser, compare them with a perceptual metric, and see what the structural
(accessibility) check can read.

| Piece | What it does |
| --- | --- |
| `spikes/parity-gpui` (this crate) | Renders the fixture through GPUI's `HeadlessAppContext` + the wgpu headless renderer, text via cosmic-text over the bundled IBM Plex Sans only (`new_without_system_fonts`), to a 780x1688 PNG (390x844 at 2x) |
| `spikes/parity-web` | The Leptos twin, rendered server-side to static HTML + CSS with the same tokens |
| `spikes/parity-compare` | SSIM (gray MSSIM) and a hybrid RGB score of two PNGs, a heat-map diff, `--min` for a CI threshold |
| Playwright (not in the repo) | Screenshots the HTML in Chromium at 390x844, deviceScaleFactor 2, and dumps `ariaSnapshot()` |

## Running it

```
# 1. GPUI, headless. Linux only at this revision (WSL Debian here); software Vulkan (lavapipe) is enough, no GPU/display.
cargo run --release -p beatseam-spike-parity-gpui -- gpui.png gpui-a11y.json
# 2. Leptos twin to HTML, then Chromium (Playwright installed in a scratch dir: npm i playwright; npx playwright install chromium)
cargo run -p beatseam-spike-parity-web -- web.html          # put the two IBMPlexSans-*.ttf next to web.html
node shot.mjs web.html web.png web-aria.txt                 # spikes/parity-web/shot.mjs: launch, goto, fonts.ready, screenshot, ariaSnapshot
# 3. Compare
cargo run --release -p beatseam-spike-parity-compare -- gpui.png web.png diff.png --min 0.95
```

## Results (2026-11-17)

| Question | Result |
| --- | --- |
| Can GPUI render headlessly in CI? | **Yes on Linux**, with no GPU and no display (WSL Debian 13, lavapipe). **No on Windows**: `gpui_platform::current_headless_renderer()` returns `None` there (only macOS and Linux are wired), so the GPUI baselines are generated on Linux. Needs the `test-support` feature. |
| Cost | First render 1.4 s (device init), later renders ~0.13 s |
| Deterministic? | Byte-identical PNGs across 3 runs. Bundled fonts only, so other OSes would shape identically too (not tried on macOS) |
| Perceptual comparison, first attempt | GPUI (Linux, headless) vs Chromium (**Windows**) SSIM **0.961**, layout/colours/fonts already matching |
| After making line heights whole-pixel tokens in both | SSIM **0.981**. The remaining differences are sub-pixel text placement; the diff shows no layout, colour or font mismatch |
| Structural check | Web side works (`ariaSnapshot`: `main`, `heading`, `progressbar`, `list`, `listitem` with names). **GPUI's accessibility tree is `None` headlessly**: `Window::debug_a11y_tree_json()` is only filled while assistive technology is connected, and the test platform never activates it |

Findings for the plan:

* The two renderers' default line heights differ a lot (GPUI's is far taller than CSS `normal`), and fractional leading
  rounds differently. Line heights, like colours and spacing, must be explicit design tokens in whole pixels.
* GPUI's flex children shrink to fit when content overflows the window, as CSS does; the fixture sets `flex_none` on the
  art box so both agree.
* The structural check cannot read GPUI's tree headlessly. Options: derive each fixture's expected structure (roles,
  names, order) from `beatseam-viewmodel`, which drives both renderers, and compare each platform's real tree to that
  (UI Automation on Windows worked in item 1; the DOM tree via Playwright works here); or force a11y active in a test
  harness, which needs a change inside GPUI and is ruled out by the no-patches decision.
* The Chromium here ran on Windows, not on a Linux CI image, so the 0.98 includes cross-OS rasterization differences;
  it should only get closer with both on Linux.
* The Leptos side was server-rendered HTML only. Hydration/wasm was not built, because parity is about DOM and CSS.
* The fixture data and tokens are duplicated by hand across the two crates; generating both from one source is the
  `beatseam-viewmodel`/`beatseam-fixtures` work in Phase 1.
