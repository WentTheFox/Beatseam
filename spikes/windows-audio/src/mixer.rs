//! The crossfade mixer: two clips, one sample-accurate equal-power gain curve, no engine automation.
//!
//! This is the part the plan says we own ("Audio: own the mixer"): the output callback pulls frames
//! from [`Mixer::render`], and the fade is plain arithmetic per frame.

use std::sync::Arc;

/// Stereo frames at the output sample rate.
pub type Frames = Arc<Vec<[f32; 2]>>;

#[derive(Clone, Copy, Debug)]
pub struct Plan {
    /// Frame (on A's timeline) where B starts and the fade begins.
    pub fade_start: usize,
    /// Fade length in frames.
    pub fade_len: usize,
}

/// Equal-power gains `(a, b)` for output frame `n`. `a² + b² == 1` during the fade.
pub fn gains(n: usize, plan: Plan) -> (f32, f32) {
    if n < plan.fade_start {
        return (1.0, 0.0);
    }
    let into = n - plan.fade_start;
    if into >= plan.fade_len {
        return (0.0, 1.0);
    }
    let t = into as f32 / plan.fade_len as f32;
    let angle = t * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

pub struct Mixer {
    a: Frames,
    b: Frames,
    plan: Plan,
    /// Next output frame (A's timeline).
    pos: usize,
    master: f32,
}

impl Mixer {
    pub fn new(a: Frames, b: Frames, plan: Plan, master: f32) -> Self {
        Self { a, b, plan, pos: 0, master }
    }

    /// Total length in frames: until B has played out.
    pub fn total_frames(&self) -> usize {
        self.plan.fade_start + self.b.len()
    }

    pub fn finished(&self) -> bool {
        self.pos >= self.total_frames()
    }

    /// Fill `out` (interleaved, `channels` per frame) and return how many frames were written.
    /// Frames past the end are silence.
    pub fn render(&mut self, out: &mut [f32], channels: usize) -> usize {
        let mut written = 0;
        for frame in out.chunks_mut(channels) {
            if self.finished() {
                frame.fill(0.0);
                continue;
            }
            let n = self.pos;
            let (ga, gb) = gains(n, self.plan);
            let a = self.a.get(n).copied().unwrap_or([0.0; 2]);
            let b = if n >= self.plan.fade_start {
                self.b.get(n - self.plan.fade_start).copied().unwrap_or([0.0; 2])
            } else {
                [0.0; 2]
            };
            let l = (a[0] * ga + b[0] * gb) * self.master;
            let r = (a[1] * ga + b[1] * gb) * self.master;
            match channels {
                1 => frame[0] = (l + r) * 0.5,
                _ => {
                    frame[0] = l;
                    frame[1] = r;
                    frame[2..].fill(0.0);
                }
            }
            self.pos += 1;
            written += 1;
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: Plan = Plan { fade_start: 100, fade_len: 50 };

    #[test]
    fn equal_power_across_the_whole_fade() {
        for n in PLAN.fade_start..PLAN.fade_start + PLAN.fade_len {
            let (a, b) = gains(n, PLAN);
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "frame {n}: {a} {b}");
        }
    }

    #[test]
    fn endpoints_are_exact() {
        assert_eq!(gains(0, PLAN), (1.0, 0.0));
        assert_eq!(gains(PLAN.fade_start - 1, PLAN), (1.0, 0.0));
        assert_eq!(gains(PLAN.fade_start, PLAN), (1.0, 0.0)); // t = 0
        assert_eq!(gains(PLAN.fade_start + PLAN.fade_len, PLAN), (0.0, 1.0));
        assert_eq!(gains(10_000, PLAN), (0.0, 1.0));
    }

    #[test]
    fn gains_are_monotonic_and_continuous() {
        let mut prev = gains(PLAN.fade_start, PLAN);
        for n in PLAN.fade_start + 1..PLAN.fade_start + PLAN.fade_len + 5 {
            let g = gains(n, PLAN);
            assert!(g.0 <= prev.0 + 1e-6 && g.1 >= prev.1 - 1e-6, "frame {n}");
            assert!((g.0 - prev.0).abs() < 0.07 && (g.1 - prev.1).abs() < 0.07, "jump at {n}");
            prev = g;
        }
    }

    #[test]
    fn render_switches_from_a_to_b_and_ends_in_silence() {
        let a: Frames = Arc::new(vec![[1.0, 1.0]; 200]);
        let b: Frames = Arc::new(vec![[0.5, 0.5]; 120]);
        let mut m = Mixer::new(a, b, PLAN, 1.0);
        let mut out = vec![0.0; 2 * 400];
        let written = m.render(&mut out, 2);
        assert_eq!(written, 100 + 120);
        assert_eq!(out[2 * 10], 1.0); // pure A before the fade
        assert_eq!(out[2 * 160], 0.5); // pure B after the fade
        assert!(out[2 * 300..].iter().all(|s| *s == 0.0)); // silence after the end
        assert!(m.finished());
    }

    #[test]
    fn render_is_independent_of_buffer_size() {
        let a: Frames = Arc::new((0..300).map(|i| [i as f32 / 300.0, 0.25]).collect());
        let b: Frames = Arc::new((0..300).map(|i| [0.1, i as f32 / 600.0]).collect());
        let all = |chunk: usize| {
            let mut m = Mixer::new(a.clone(), b.clone(), PLAN, 0.8);
            let mut v = Vec::new();
            while !m.finished() {
                let mut buf = vec![0.0; 2 * chunk];
                let n = m.render(&mut buf, 2);
                v.extend_from_slice(&buf[..2 * n]);
            }
            v
        };
        assert_eq!(all(1), all(64));
        assert_eq!(all(64), all(333));
    }
}
