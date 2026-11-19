//! Phase 0 spike, item 3 (web half) of docs/rust-gpui-rewrite-plan.md: the *same* mixer source file the
//! Windows spike uses, compiled to a plain wasm module (C ABI, no imports) and driven from an
//! `AudioWorkletProcessor` in 128-frame quanta.
//!
//! Test material is synthetic (a 220 Hz and a 330 Hz sine) so the output can be checked exactly; decoding is
//! a separate question (symphonia is pure Rust and the Windows spike already proves it).

#[path = "../../windows-audio/src/mixer.rs"]
#[allow(dead_code)]
mod mixer;

use std::sync::Arc;

use mixer::{Mixer, Plan};

const QUANTUM: usize = 128;

pub struct Engine {
    mixer: Mixer,
    /// Planar output the JS side reads after each `engine_render`.
    left: [f32; QUANTUM],
    right: [f32; QUANTUM],
    scratch: Vec<f32>,
}

fn sine(rate: u32, secs: f32, hz: f32) -> Vec<[f32; 2]> {
    (0..(rate as f32 * secs) as usize)
        .map(|i| {
            let s = (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5;
            [s, s]
        })
        .collect()
}

#[unsafe(no_mangle)]
pub extern "C" fn engine_new(rate: u32, secs: f32, fade_at: f32, fade: f32) -> *mut Engine {
    let plan = Plan { fade_start: (fade_at * rate as f32) as usize, fade_len: (fade * rate as f32) as usize };
    let mixer = Mixer::new(Arc::new(sine(rate, secs, 220.0)), Arc::new(sine(rate, secs, 330.0)), plan, 1.0);
    Box::into_raw(Box::new(Engine { mixer, left: [0.0; QUANTUM], right: [0.0; QUANTUM], scratch: vec![0.0; 2 * QUANTUM] }))
}

/// Render one quantum into the planar buffers; returns frames written (0 once finished).
#[unsafe(no_mangle)]
pub extern "C" fn engine_render(e: *mut Engine) -> u32 {
    let e = unsafe { &mut *e };
    let n = e.mixer.render(&mut e.scratch, 2);
    for i in 0..QUANTUM {
        e.left[i] = e.scratch[2 * i];
        e.right[i] = e.scratch[2 * i + 1];
    }
    n as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn engine_left(e: *mut Engine) -> *const f32 {
    unsafe { (*e).left.as_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn engine_right(e: *mut Engine) -> *const f32 {
    unsafe { (*e).right.as_ptr() }
}
