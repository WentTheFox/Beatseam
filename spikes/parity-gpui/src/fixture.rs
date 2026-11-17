//! A deterministic "now playing" fixture: fixed state, no clock, no random ids, no animation.
//! The design tokens here are the ones the Leptos twin would get as CSS custom properties.

use gpui::{Context, FontWeight, IntoElement, Render, Window, div, prelude::*, px, rgb};

pub mod tokens {
    pub const BG: u32 = 0x1e1e2e;
    pub const SURFACE: u32 = 0x313244;
    pub const SURFACE_ALT: u32 = 0x181825;
    pub const TEXT: u32 = 0xcdd6f4;
    pub const TEXT_DIM: u32 = 0x9399b2;
    pub const ACCENT: u32 = 0x89b4fa;
    pub const FONT: &str = "IBM Plex Sans";
}

struct Track {
    title: &'static str,
    artist: &'static str,
    duration: &'static str,
}

pub struct NowPlaying {
    tracks: Vec<Track>,
    current: usize,
    progress: f32,
}

impl NowPlaying {
    pub fn fixture() -> Self {
        let t = |title, artist, duration| Track { title, artist, duration };
        Self {
            tracks: vec![
                t("Mood", "24kGoldn", "2:21"),
                t("Ayo Technology", "50 Cent", "4:06"),
                t("Aether", "&I", "5:12"),
                t("Dive", "&I", "6:03"),
                t("Enchantress", "3SPIRIT", "5:48"),
            ],
            current: 1,
            progress: 0.4,
        }
    }
}

impl Render for NowPlaying {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let now = &self.tracks[self.current];
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(20.))
            .bg(rgb(tokens::BG))
            .text_color(rgb(tokens::TEXT))
            .font_family(tokens::FONT)
            .text_size(px(16.))
            // cover art placeholder
            .child(
                div()
                    .w_full()
                    .h(px(240.))
                    .rounded(px(16.))
                    .bg(rgb(tokens::SURFACE))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(px(120.)).rounded_full().bg(rgb(tokens::SURFACE_ALT)).border_4().border_color(rgb(tokens::ACCENT))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_size(px(24.)).font_weight(FontWeight::SEMIBOLD).child(now.title))
                    .child(div().text_color(rgb(tokens::TEXT_DIM)).child(now.artist)),
            )
            // progress
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div().w_full().h(px(6.)).rounded_full().bg(rgb(tokens::SURFACE)).child(
                            div().h_full().w(gpui::relative(self.progress)).rounded_full().bg(rgb(tokens::ACCENT)),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(13.))
                            .text_color(rgb(tokens::TEXT_DIM))
                            .child("1:38")
                            .child(now.duration),
                    ),
            )
            // up next
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Up next"))
            .children(self.tracks.iter().enumerate().map(|(i, t)| {
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .when(i == self.current, |d| d.bg(rgb(tokens::SURFACE)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().child(t.title))
                            .child(div().text_size(px(13.)).text_color(rgb(tokens::TEXT_DIM)).child(t.artist)),
                    )
                    .child(div().text_size(px(13.)).text_color(rgb(tokens::TEXT_DIM)).child(t.duration))
            }))
    }
}
