//! Phase 0 spike, item 3 (docs/rust-gpui-rewrite-plan.md): symphonia decode + cpal output + our own
//! equal-power crossfade mixer, on Windows.
//!
//! ```text
//! cargo run --release -p beatseam-spike-windows-audio -- A.mp3 B.mp3 [--fade-at 20] [--fade 6]
//!     [--secs 40]      seconds decoded from each file (default 40)
//!     [--render out.wav]  render offline with the same mixer instead of playing (no sound)
//!     [--volume 0.3]   master gain for playback
//! ```

mod decode;
mod mixer;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::{AtomicU64, AtomicUsize, Ordering}};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use mixer::{Mixer, Plan};

struct Args {
    a: PathBuf,
    b: PathBuf,
    fade_at: f32,
    fade: f32,
    secs: f32,
    render: Option<PathBuf>,
    volume: f32,
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let (mut files, mut args) = (Vec::new(), Args {
        a: PathBuf::new(), b: PathBuf::new(), fade_at: 20.0, fade: 6.0, secs: 40.0, render: None, volume: 0.3,
    });
    while let Some(arg) = it.next() {
        let mut num = |name: &str| it.next().and_then(|v| v.parse::<f32>().ok()).ok_or(format!("{name} needs a number"));
        match arg.as_str() {
            "--fade-at" => args.fade_at = num("--fade-at")?,
            "--fade" => args.fade = num("--fade")?,
            "--secs" => args.secs = num("--secs")?,
            "--volume" => args.volume = num("--volume")?,
            "--render" => args.render = Some(it.next().ok_or("--render needs a path")?.into()),
            _ => files.push(PathBuf::from(arg)),
        }
    }
    if files.len() != 2 {
        return Err("usage: A B [--fade-at S] [--fade S] [--secs S] [--render out.wav] [--volume V]".into());
    }
    args.b = files.pop().unwrap();
    args.a = files.pop().unwrap();
    Ok(args)
}

fn main() -> Result<(), String> {
    let args = parse()?;
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let (out_rate, channels) = (config.sample_rate(), config.channels() as usize);
    println!(
        "output: {} | {} Hz, {} ch, {:?}",
        device.description().map(|d| d.name().to_string()).unwrap_or_default(),
        out_rate, channels, config.sample_format()
    );

    let rate = if args.render.is_some() { 48_000 } else { out_rate };
    let a = decode::decode(&args.a, rate, args.secs)?;
    let b = decode::decode(&args.b, rate, args.secs)?;
    for (name, d) in [("A", &a), ("B", &b)] {
        println!(
            "{name}: {} Hz {} ch -> {:.1}s at {rate} Hz, decoded in {} ms",
            d.source_rate, d.source_channels, d.frames.len() as f32 / rate as f32, d.decode_ms
        );
    }
    let plan = Plan {
        fade_start: (args.fade_at * rate as f32) as usize,
        fade_len: (args.fade * rate as f32) as usize,
    };
    if plan.fade_start + plan.fade_len > a.frames.len() {
        return Err(format!("A is only {:.1}s long; lower --fade-at/--fade or raise --secs", a.frames.len() as f32 / rate as f32));
    }

    if let Some(path) = &args.render {
        return render_offline(path, a.frames, b.frames, plan, rate);
    }
    play(device, config, channels, Mixer::new(a.frames, b.frames, plan, args.volume), plan, rate)
}

/// Offline render: same mixer, no audio device. Also prints the loudness around the fade so the
/// "no dip, no bump" claim can be checked without listening.
fn render_offline(path: &PathBuf, a: mixer::Frames, b: mixer::Frames, plan: Plan, rate: u32) -> Result<(), String> {
    let mut m = Mixer::new(a, b, plan, 1.0);
    let mut out = vec![0.0f32; 2 * m.total_frames()];
    m.render(&mut out, 2);
    let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    let mut peak = 0.0f32;
    for s in &out {
        peak = peak.max(s.abs());
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())?;
    // RMS in 0.5 s windows across the fade.
    let win = rate as usize / 2;
    println!("peak {peak:.3}; RMS per 0.5 s window around the fade (start at {:.1}s, {:.1}s long):",
        plan.fade_start as f32 / rate as f32, plan.fade_len as f32 / rate as f32);
    let from = plan.fade_start.saturating_sub(2 * win) / win;
    let to = (plan.fade_start + plan.fade_len) / win + 3;
    for w in from..to {
        let seg = &out[2 * w * win..(2 * (w + 1) * win).min(out.len())];
        let rms = (seg.iter().map(|s| s * s).sum::<f32>() / seg.len().max(1) as f32).sqrt();
        println!("  {:5.1}s  {rms:.3}  {}", w as f32 * 0.5, "#".repeat((rms * 80.0) as usize));
    }
    println!("wrote {}", path.display());
    Ok(())
}

fn play(device: cpal::Device, config: cpal::SupportedStreamConfig, channels: usize, mixer: Mixer, plan: Plan, rate: u32) -> Result<(), String> {
    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("spike only handles f32 output, device gives {:?}", config.sample_format()));
    }
    let total = mixer.total_frames();
    let mixer = Arc::new(Mutex::new(mixer));
    let callbacks = Arc::new(AtomicU64::new(0));
    let worst_us = Arc::new(AtomicU64::new(0));
    let worst_ratio_permille = Arc::new(AtomicU64::new(0));
    let frames_out = Arc::new(AtomicUsize::new(0));

    let (m, cb, wu, wr, fo) = (mixer.clone(), callbacks.clone(), worst_us.clone(), worst_ratio_permille.clone(), frames_out.clone());
    let stream_config: cpal::StreamConfig = config.into();
    let stream = device
        .build_output_stream(
            stream_config,
            move |data: &mut [f32], _| {
                let t = Instant::now();
                let n = m.lock().unwrap().render(data, channels);
                fo.fetch_add(n, Ordering::Relaxed);
                cb.fetch_add(1, Ordering::Relaxed);
                let spent = t.elapsed().as_micros() as u64;
                wu.fetch_max(spent, Ordering::Relaxed);
                let budget = (data.len() / channels) as u64 * 1_000_000 / rate as u64;
                wr.fetch_max(spent * 1000 / budget.max(1), Ordering::Relaxed);
            },
            |e| eprintln!("stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;

    println!("playing {:.1}s: A, then a {:.1}s equal-power fade at {:.1}s into B (volume is low)...",
        total as f32 / rate as f32, plan.fade_len as f32 / rate as f32, plan.fade_start as f32 / rate as f32);
    stream.play().map_err(|e| e.to_string())?;
    let started = Instant::now();
    while !mixer.lock().unwrap().finished() {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300)); // let the device buffer drain
    drop(stream);
    println!(
        "done in {:.1}s wall clock (expected {:.1}s); {} callbacks, {} frames; worst callback {} us = {:.1}% of its buffer time",
        started.elapsed().as_secs_f32(), total as f32 / rate as f32,
        callbacks.load(Ordering::Relaxed), frames_out.load(Ordering::Relaxed),
        worst_us.load(Ordering::Relaxed), worst_ratio_permille.load(Ordering::Relaxed) as f32 / 10.0
    );
    Ok(())
}
