//! Phase 0 spike, item 6 of docs/rust-gpui-rewrite-plan.md: can GPUI render a fixture view to a PNG with
//! no window and no display, so the visual parity suite can run in CI?
//!
//! At the pinned Zed revision GPUI's headless renderer exists for Linux (wgpu) and macOS only;
//! `gpui_platform::current_headless_renderer()` returns `None` on Windows. Run it on Linux/WSL:
//!
//! ```text
//! cargo run --release -p beatseam-spike-parity-gpui -- out.png
//! ```
//!
//! Text goes through cosmic-text with *only* the bundled IBM Plex Sans (no system fonts), so the same
//! fixture shapes identically on every OS.

#[cfg(target_os = "linux")]
mod fixture;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use std::borrow::Cow;
    use std::sync::Arc;

    use gpui::{AppContext as _, HeadlessAppContext, PlatformTextSystem, px, size};
    use gpui_wgpu::CosmicTextSystem;

    let out = std::env::args().nth(1).unwrap_or_else(|| "fixture.png".into());

    let text_system = Arc::new(CosmicTextSystem::new_without_system_fonts("IBM Plex Sans"));
    text_system.add_fonts(vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf").as_slice()),
    ])?;

    let started = std::time::Instant::now();
    let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(()), || {
        gpui_platform::current_headless_renderer()
    });
    let window = cx.open_window(size(px(390.), px(844.)), |_, cx| cx.new(|_| fixture::NowPlaying::fixture()))?;
    cx.run_until_parked();
    let tree = window.update(&mut cx, |_, window, _| window.debug_a11y_tree_json())?;
    println!("a11y tree after first frame: {}", tree.as_deref().map(|t| format!("{} bytes", t.len())).unwrap_or_else(|| "None (a11y not active headlessly)".into()));
    if let (Some(t), Some(path)) = (tree, std::env::args().nth(2)) {
        std::fs::write(path, t)?;
    }
    let image = cx.capture_screenshot(window.into())?;
    println!("rendered {}x{} in {} ms", image.width(), image.height(), started.elapsed().as_millis());
    image.save(&out)?;
    println!("wrote {out}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("GPUI's headless renderer is only wired for Linux and macOS at this revision; run this on Linux/WSL.");
    std::process::exit(2);
}
