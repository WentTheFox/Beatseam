//! Phase 0 spike, item 6: the Leptos twin of `spikes/parity-gpui`'s now-playing fixture.
//!
//! Server-side rendering only: Leptos produces the DOM as an HTML string, and a headless Chromium
//! (Playwright) screenshots it. Hydration/wasm is not what parity is about, so it is left out here.
//! The tokens below are the same values as `parity-gpui/src/fixture.rs::tokens`; in the real plan both
//! are generated from `beatseam-viewmodel`.

use leptos::prelude::*;
use leptos::tachys::view::RenderHtml;

const BG: &str = "#1e1e2e";
const SURFACE: &str = "#313244";
const SURFACE_ALT: &str = "#181825";
const TEXT: &str = "#cdd6f4";
const TEXT_DIM: &str = "#9399b2";
const ACCENT: &str = "#89b4fa";

struct Track {
    title: &'static str,
    artist: &'static str,
    duration: &'static str,
}

const TRACKS: [Track; 5] = [
    Track { title: "Mood", artist: "24kGoldn", duration: "2:21" },
    Track { title: "Ayo Technology", artist: "50 Cent", duration: "4:06" },
    Track { title: "Aether", artist: "&I", duration: "5:12" },
    Track { title: "Dive", artist: "&I", duration: "6:03" },
    Track { title: "Enchantress", artist: "3SPIRIT", duration: "5:48" },
];
const CURRENT: usize = 1;
const PROGRESS: f32 = 0.4;

#[component]
fn NowPlaying() -> impl IntoView {
    let now = &TRACKS[CURRENT];
    view! {
        <main class="screen" role="main" aria-label="Now playing">
            <div class="art"><div class="disc"></div></div>
            <div class="meta">
                <h1 class="title">{now.title}</h1>
                <div class="dim">{now.artist}</div>
            </div>
            <div class="progress-block">
                <div class="bar" role="progressbar" aria-label="Playback position" aria-valuemin="0" aria-valuemax="100" aria-valuenow="40">
                    <div class="fill" style=format!("width:{}%", PROGRESS * 100.0)></div>
                </div>
                <div class="times small dim"><span>"1:38"</span><span>{now.duration}</span></div>
            </div>
            <h2 class="heading">"Up next"</h2>
            <ul class="list" aria-label="Up next">
                {TRACKS.iter().enumerate().map(|(i, t)| view! {
                    <li class="row" class:current=move || i == CURRENT aria-current=if i == CURRENT { "true" } else { "false" }>
                        <div class="col"><div>{t.title}</div><div class="small dim">{t.artist}</div></div>
                        <div class="small dim">{t.duration}</div>
                    </li>
                }).collect_view()}
            </ul>
        </main>
    }
}

fn css() -> String {
    format!(
        r#"
@font-face {{ font-family: "IBM Plex Sans"; font-weight: 400; src: url("IBMPlexSans-Regular.ttf"); }}
@font-face {{ font-family: "IBM Plex Sans"; font-weight: 600; src: url("IBMPlexSans-SemiBold.ttf"); }}
*, *::before, *::after {{ box-sizing: border-box; margin: 0; }}
html, body {{ height: 100%; background: {BG}; color: {TEXT}; font: 400 16px "IBM Plex Sans"; line-height: 21px; }}
.screen {{ min-height: 100%; display: flex; flex-direction: column; gap: 16px; padding: 20px; }}
.art {{ width: 100%; height: 240px; border-radius: 16px; background: {SURFACE}; display: flex; align-items: center; justify-content: center; }}
.disc {{ width: 120px; height: 120px; border-radius: 9999px; background: {SURFACE_ALT}; border: 4px solid {ACCENT}; }}
.meta {{ display: flex; flex-direction: column; gap: 4px; }}
.title {{ font-size: 24px; line-height: 31px; font-weight: 600; }}
.dim {{ color: {TEXT_DIM}; }}
.small {{ font-size: 13px; line-height: 17px; }}
.progress-block {{ display: flex; flex-direction: column; gap: 6px; }}
.bar {{ width: 100%; height: 6px; border-radius: 9999px; background: {SURFACE}; }}
.fill {{ height: 100%; border-radius: 9999px; background: {ACCENT}; }}
.times {{ display: flex; justify-content: space-between; }}
.heading {{ font-size: 16px; font-weight: 600; }}
.list {{ list-style: none; padding: 0; display: flex; flex-direction: column; }}
.row {{ display: flex; justify-content: space-between; align-items: center; padding: 10px 12px; border-radius: 10px; }}
.row.current {{ background: {SURFACE}; }}
.col {{ display: flex; flex-direction: column; }}
"#
    )
}

fn main() {
    let body = view! { <NowPlaying/> }.to_html();
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Now playing</title><style>{}</style></head><body>{}</body></html>",
        css(),
        body
    );
    let out = std::env::args().nth(1).unwrap_or_else(|| "fixture.html".into());
    std::fs::write(&out, html).expect("write html");
    println!("wrote {out}");
}
