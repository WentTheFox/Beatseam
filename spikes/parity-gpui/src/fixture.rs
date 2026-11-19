//! A deterministic "now playing" fixture: fixed state, no clock, no random ids, no animation.
//! The design tokens here are the ones the Leptos twin gets as CSS custom properties (copied by hand in this
//! spike; generated from `beatseam-viewmodel` in the real plan).

use gpui::{Context, FontWeight, IntoElement, Render, Role, Window, div, prelude::*, px, relative, rgb, text};

pub mod tokens {
    pub const BG: u32 = 0x1e1e2e;
    pub const SURFACE: u32 = 0x313244;
    pub const SURFACE_ALT: u32 = 0x181825;
    pub const TEXT: u32 = 0xcdd6f4;
    pub const TEXT_DIM: u32 = 0x9399b2;
    pub const ACCENT: u32 = 0x89b4fa;
    pub const FONT: &str = "IBM Plex Sans";
    /// Explicit leading in whole pixels: the two renderers' *defaults* differ a lot (GPUI's is far taller than CSS
    /// `normal`) and a fractional multiplier rounds differently, so line heights are tokens in px.
    pub const LH_BODY: f32 = 21.0;
    pub const LH_SMALL: f32 = 17.0;
    pub const LH_TITLE: f32 = 31.0;
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
            .id("screen")
            .role(Role::Main)
            .aria_label("Now playing")
            .size_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(20.))
            .bg(rgb(tokens::BG))
            .text_color(rgb(tokens::TEXT))
            .font_family(tokens::FONT)
            .text_size(px(16.))
            .line_height(px(tokens::LH_BODY))
            // cover art placeholder
            .child(
                div()
                    .w_full()
                    .h(px(240.))
                    .flex_none()
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
                    .child(
                        div()
                            .id("title")
                            .role(Role::Heading)
                            .aria_level(1)
                            .aria_label(now.title)
                            .text_size(px(24.))
                            .line_height(px(tokens::LH_TITLE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(text!(now.title)),
                    )
                    .child(div().id("artist").text_color(rgb(tokens::TEXT_DIM)).child(text!(now.artist))),
            )
            // progress
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .id("position")
                            .role(Role::ProgressIndicator)
                            .aria_label("Playback position")
                            .aria_min_numeric_value(0.0)
                            .aria_max_numeric_value(100.0)
                            .aria_numeric_value(self.progress as f64 * 100.0)
                            .w_full()
                            .h(px(6.))
                            .rounded_full()
                            .bg(rgb(tokens::SURFACE))
                            .child(div().h_full().w(relative(self.progress)).rounded_full().bg(rgb(tokens::ACCENT))),
                    )
                    .child(
                        div()
                            .id("times")
                            .flex()
                            .justify_between()
                            .text_size(px(13.)).line_height(px(tokens::LH_SMALL))
                            .text_color(rgb(tokens::TEXT_DIM))
                            .child(div().id("elapsed").child(text!("1:38")))
                            .child(div().id("total").child(text!(now.duration))),
                    ),
            )
            // up next
            .child(
                div()
                    .id("up-next-heading")
                    .role(Role::Heading)
                    .aria_level(2)
                    .aria_label("Up next")
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(text!("Up next")),
            )
            .child(
                div()
                    .id("up-next")
                    .role(Role::List)
                    .aria_label("Up next")
                    .flex()
                    .flex_col()
                    .children(self.tracks.iter().enumerate().map(|(i, t)| {
                        div()
                            .id(("row", i))
                            .role(Role::ListItem)
                            .aria_label(format!("{} {} {}", t.title, t.artist, t.duration))
                            .flex()
                            .justify_between()
                            .items_center()
                            .px(px(12.))
                            .py(px(10.))
                            .rounded(px(10.))
                            .when(i == self.current, |d| d.bg(rgb(tokens::SURFACE)))
                            .child(
                                div()
                                    .id(("row-text", i))
                                    .flex()
                                    .flex_col()
                                    .child(div().id(("row-title", i)).child(text!(t.title)))
                                    .child(div().id(("row-artist", i)).text_size(px(13.)).line_height(px(tokens::LH_SMALL)).text_color(rgb(tokens::TEXT_DIM)).child(text!(t.artist))),
                            )
                            .child(div().id(("row-dur", i)).text_size(px(13.)).line_height(px(tokens::LH_SMALL)).text_color(rgb(tokens::TEXT_DIM)).child(text!(t.duration)))
                    })),
            )
    }
}
