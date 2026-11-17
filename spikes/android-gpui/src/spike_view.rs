//! Phase 0 spike (docs/rust-gpui-rewrite-plan.md, item 2): GPUI on Android.
//! Same view as spikes/windows-gpui, ported to the `gpui-pre` snapshot the Android backend uses.
//!
//! One window with the four things the plan asks for, each with GPUI's accessibility attributes
//! set the way a real screen would set them, so Narrator / UI Automation can be pointed at it:
//!   * a scrolling list of 5,000 rows (`uniform_list`, `Role::List` / `Role::ListItem`)
//!   * a text input (copied from GPUI's own example, plus `Role::TextInput`)
//!   * a custom-drawn rotating record + tonearm (`canvas`), labelled as one image
//!   * a slider (`Role::Slider`, with Increment/Decrement actions and keyboard arrows)
//!
//! Throwaway: results go back into the plan's Phase 0 section.

use crate::text_input;
use std::time::Instant;

use gpui::{
    AccessibleAction, App, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, Orientation, PathBuilder, Role, SharedString,
    Window, WindowOptions, actions, canvas, div, fill, point, prelude::*, px, rgb,
    size, text, uniform_list,
};
use crate::text_input::TextInput;

const ROWS: usize = 5_000;

actions!(
    spike,
    [Quit, SliderLeft, SliderRight, SliderHome, SliderEnd, TabNext, TabPrev]
);

struct Spike {
    input: Entity<TextInput>,
    slider_focus: FocusHandle,
    slider: f64, // 0..=100
    dragging: bool,
    slider_bounds: Bounds<gpui::Pixels>,
    selected_row: Option<usize>,
    started: Instant,
}

impl Spike {
    fn set_slider(&mut self, v: f64, cx: &mut Context<Self>) {
        self.slider = v.clamp(0.0, 100.0);
        cx.notify();
    }

    fn slider_from_x(&mut self, x: gpui::Pixels, cx: &mut Context<Self>) {
        let b = self.slider_bounds;
        if b.size.width > px(0.) {
            let frac = f32::from((x - b.origin.x) / b.size.width) as f64;
            self.set_slider(frac * 100.0, cx);
        }
    }
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep the record spinning: one turn every 3 s, driven by wall-clock time.
        window.request_animation_frame();
        let turns = (self.started.elapsed().as_secs_f32() / 3.0).fract();
        let this = cx.entity().downgrade();

        div()
            .id("root")
            .role(Role::Application)
            .aria_label("Beatseam GPUI accessibility spike")
            .on_action(|_: &TabNext, window, cx| window.focus_next(cx))
            .on_action(|_: &TabPrev, window, cx| window.focus_prev(cx))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .id("heading")
                    .role(Role::Heading)
                    .aria_level(1)
                    .aria_label("Now playing spike")
                    .text_xl()
                    .child(text!("Now playing spike")),
            )
            // Text input
            .child(self.input.clone())
            // Record + tonearm, one labelled image
            .child(
                div()
                    .id("record")
                    .role(Role::Image)
                    .aria_label("Record spinning, tonearm on the record")
                    .w(px(220.))
                    .h(px(220.))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let c = bounds.center();
                                let r = f32::from(bounds.size.width.min(bounds.size.height)) / 2.0;
                                let angle = turns * std::f32::consts::TAU;
                                // Disc: a fan of wedges so the rotation is visible.
                                for i in 0..24 {
                                    let a0 = angle + i as f32 * std::f32::consts::TAU / 24.0;
                                    let a1 = a0 + std::f32::consts::TAU / 24.0;
                                    let mut pb = PathBuilder::fill();
                                    pb.move_to(c);
                                    pb.line_to(point(
                                        c.x + px(r * a0.cos()),
                                        c.y + px(r * a0.sin()),
                                    ));
                                    pb.line_to(point(
                                        c.x + px(r * a1.cos()),
                                        c.y + px(r * a1.sin()),
                                    ));
                                    pb.close();
                                    if let Ok(path) = pb.build() {
                                        let shade = if i % 2 == 0 { 0x181825 } else { 0x313244 };
                                        window.paint_path(path, rgb(shade));
                                    }
                                }
                                // Tonearm: a bar pivoting from the top-right corner.
                                let pivot = point(
                                    bounds.origin.x + bounds.size.width - px(8.),
                                    bounds.origin.y + px(8.),
                                );
                                let swing = -2.3 + 0.2 * (turns * std::f32::consts::TAU).sin();
                                let tip = point(
                                    pivot.x + px(r * 1.1 * swing.cos()),
                                    pivot.y - px(r * 1.1 * swing.sin()),
                                );
                                let mut pb = PathBuilder::stroke(px(6.));
                                pb.move_to(pivot);
                                pb.line_to(tip);
                                if let Ok(path) = pb.build() {
                                    window.paint_path(path, rgb(0x89b4fa));
                                }
                                window.paint_quad(fill(
                                    Bounds::centered_at(c, size(px(24.), px(24.))),
                                    rgb(0xf38ba8),
                                ));
                            },
                        )
                        .size_full(),
                    ),
            )
            // Slider
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(text!("Volume"))
                    .child({
                        let (inc, dec) = (this.clone(), this.clone());
                        let measure = this.clone();
                        div()
                            .id("volume")
                            .focusable()
                            .tab_stop(true)
                            .role(Role::Slider)
                            .aria_label("Volume")
                            .aria_orientation(Orientation::Horizontal)
                            .aria_numeric_value(self.slider)
                            .aria_min_numeric_value(0.0)
                            .aria_max_numeric_value(100.0)
                            .aria_numeric_value_step(5.0)
                            .track_focus(&self.slider_focus)
                            .key_context("Slider")
                            .on_action(cx.listener(|s, _: &SliderLeft, _, cx| {
                                s.set_slider(s.slider - 5.0, cx)
                            }))
                            .on_action(cx.listener(|s, _: &SliderRight, _, cx| {
                                s.set_slider(s.slider + 5.0, cx)
                            }))
                            .on_action(cx.listener(|s, _: &SliderHome, _, cx| s.set_slider(0.0, cx)))
                            .on_action(cx.listener(|s, _: &SliderEnd, _, cx| s.set_slider(100.0, cx)))
                            .on_a11y_action(AccessibleAction::Increment, move |_, _, cx| {
                                inc.update(cx, |s, cx| s.set_slider(s.slider + 5.0, cx)).ok();
                            })
                            .on_a11y_action(AccessibleAction::Decrement, move |_, _, cx| {
                                dec.update(cx, |s, cx| s.set_slider(s.slider - 5.0, cx)).ok();
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|s, e: &MouseDownEvent, window, cx| {
                                    s.dragging = true;
                                    window.focus(&s.slider_focus, cx);
                                    s.slider_from_x(e.position.x, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(|s, e: &MouseMoveEvent, _, cx| {
                                if s.dragging {
                                    s.slider_from_x(e.position.x, cx);
                                }
                            }))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|s, _, _, _| s.dragging = false),
                            )
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(|s, _, _, _| s.dragging = false),
                            )
                            .w(px(240.))
                            .h(px(28.))
                            .rounded_md()
                            .bg(rgb(0x45475a))
                            .child(
                                canvas(
                                    move |bounds, _, cx| {
                                        measure.update(cx, |s, _| s.slider_bounds = bounds).ok();
                                    },
                                    |_, _, _, _| (),
                                )
                                .absolute()
                                .size_full(),
                            )
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative((self.slider / 100.0) as f32))
                                    .rounded_md()
                                    .bg(rgb(0x89b4fa)),
                            )
                    })
                    .child(text!(format!("{:.0}", self.slider))),
            )
            // 5k-row list
            .child(
                div()
                    .id("tracks")
                    .role(Role::List)
                    .aria_label("Tracks")
                    .aria_size_of_set(ROWS)
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        uniform_list(
                            "track-rows",
                            ROWS,
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .map(|ix: usize| {
                                        let label: SharedString =
                                            format!("Track {} - Artist {}", ix + 1, ix % 97).into();
                                        let selected = this.selected_row == Some(ix);
                                        div()
                                            .id(("track", ix))
                                            .focusable()
                                            .tab_stop(false)
                                            .role(Role::ListItem)
                                            .aria_label(label.clone())
                                            .aria_position_in_set(ix + 1)
                                            .aria_size_of_set(ROWS)
                                            .aria_selected(selected)
                                            .px_2()
                                            .py_1()
                                            .cursor_pointer()
                                            .when(selected, |d| d.bg(rgb(0x585b70)))
                                            .on_click(cx.listener(move |s, _, _, cx| {
                                                s.selected_row = Some(ix);
                                                cx.notify();
                                            }))
                                            .child(text!(label.clone()))
                                    })
                                    .collect()
                            }),
                        )
                        .h_full(),
                    ),
            )
    }
}

impl Focusable for Spike {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

pub fn open(cx: &mut App) {
    {
        cx.bind_keys([
            KeyBinding::new("tab", TabNext, None),
            KeyBinding::new("shift-tab", TabPrev, None),
            KeyBinding::new("left", SliderLeft, Some("Slider")),
            KeyBinding::new("right", SliderRight, Some("Slider")),
            KeyBinding::new("home", SliderHome, Some("Slider")),
            KeyBinding::new("end", SliderEnd, Some("Slider")),
            KeyBinding::new("backspace", text_input::Backspace, Some("TextInput")),
            KeyBinding::new("delete", text_input::Delete, Some("TextInput")),
            KeyBinding::new("left", text_input::Left, Some("TextInput")),
            KeyBinding::new("right", text_input::Right, Some("TextInput")),
            KeyBinding::new("shift-left", text_input::SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", text_input::SelectRight, Some("TextInput")),
            KeyBinding::new("ctrl-a", text_input::SelectAll, Some("TextInput")),
            KeyBinding::new("ctrl-v", text_input::Paste, Some("TextInput")),
            KeyBinding::new("ctrl-c", text_input::Copy, Some("TextInput")),
            KeyBinding::new("ctrl-x", text_input::Cut, Some("TextInput")),
            KeyBinding::new("home", text_input::Home, Some("TextInput")),
            KeyBinding::new("end", text_input::End, Some("TextInput")),
            KeyBinding::new("alt-f4", Quit, None),
        ]);

                cx.open_window(
            WindowOptions {
                window_bounds: None,
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Beatseam GPUI spike".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let input = cx.new(|cx| TextInput::new("Search tracks", cx));
                let slider_focus = cx.focus_handle();
                window.focus(&input.focus_handle(cx), cx);
                cx.new(|_| Spike {
                    input,
                    slider_focus,
                    slider: 60.0,
                    dragging: false,
                    slider_bounds: Bounds::default(),
                    selected_row: None,
                    started: Instant::now(),
                })
            },
        )
        .unwrap();
        cx.activate(true);
    }
}
