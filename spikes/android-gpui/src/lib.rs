//! Phase 0 spike, item 2 of docs/rust-gpui-rewrite-plan.md: the same view as `spikes/windows-gpui`
//! running on Android through the community `gpui-mobile` backend, used as-is.
//!
//! Built with `cargo ndk` and loaded by the backend's own example host (the Gradle project in
//! `example/android/gradle` of longbridge/gpui-mobile), which expects the library name
//! `gpui_mobile_example`. See README.md.

extern crate gpui_mobile;

mod fs_probe;
mod spike_view;
mod text_input;

#[cfg(target_os = "android")]
use gpui::Application;
#[cfg(target_os = "android")]
use gpui_mobile::android::jni;

/// Called by `android-activity` on a dedicated native thread; does not return until the app exits.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("beatseam-spike"),
    );
    jni::install_panic_hook();
    std::thread::spawn(fs_probe::run);
    let _platform = jni::init_platform(&app);
    let Some(shared) = jni::shared_platform() else {
        log::error!("shared_platform() returned None");
        return;
    };
    Application::with_platform(shared.into_rc()).run(|cx| {
        spike_view::open(cx);
    });
}
