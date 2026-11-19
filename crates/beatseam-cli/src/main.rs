//! `beatseam`: a debug CLI over the Rust core. No UI, just proof the core, the library scan and the audio engine work together.
//!
//! ```text
//! beatseam scan   <folder>
//! beatseam play   <folder> [--playlist N|name] [--crossfade S] [--volume V] [--shuffle] [--secs S]
//! beatseam render <folder> --out out.wav [--playlist N|name] [--crossfade S] [--secs S]
//! ```
#![allow(clippy::type_complexity, clippy::arc_with_non_send_sync, clippy::too_many_arguments)] // see beatseam-core's lib.rs

mod runtime;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beatseam_audio::MixerEngine;
use beatseam_core::file_access::{FileRef, RootKind};
use beatseam_core::http::block_on;
use beatseam_core::library_scan::{ScanProgress, ScanResult, scan_root};
use beatseam_core::library_store::TrackRecord;
use beatseam_core::playback::{
    PlaylistOptions, PlaylistPlayer, PlaylistPlayerOptions, ResolveTrack, TrackPlayerStatus,
};
use beatseam_core::tasks::Runtime;
use beatseam_native::{SqliteLibraryStore, StdFileAccess};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use runtime::CliRuntime;

const USAGE: &str = "usage: beatseam <scan|play|render> <folder> [--playlist N|name] [--crossfade S] [--volume V] [--shuffle] [--out file.wav] [--secs S]";

struct Options {
    command: String,
    folder: PathBuf,
    playlist: Option<String>,
    crossfade: f64,
    volume: f64,
    shuffle: bool,
    out: Option<PathBuf>,
    secs: Option<f64>,
}

fn parse() -> Result<Options, String> {
    let mut it = std::env::args().skip(1);
    let command = it.next().ok_or(USAGE)?;
    let folder = PathBuf::from(it.next().ok_or(USAGE)?);
    let mut o = Options {
        command,
        folder,
        playlist: None,
        crossfade: 5.0,
        volume: 0.3,
        shuffle: false,
        out: None,
        secs: None,
    };
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        let number = |name: &str, v: String| {
            v.parse::<f64>()
                .map_err(|_| format!("{name} needs a number"))
        };
        match arg.as_str() {
            "--playlist" => o.playlist = Some(value("--playlist")?),
            "--crossfade" => o.crossfade = number("--crossfade", value("--crossfade")?)?,
            "--volume" => o.volume = number("--volume", value("--volume")?)?,
            "--secs" => o.secs = Some(number("--secs", value("--secs")?)?),
            "--out" => o.out = Some(value("--out")?.into()),
            "--shuffle" => o.shuffle = true,
            _ => return Err(format!("unknown argument {arg}\n{USAGE}")),
        }
    }
    Ok(o)
}

fn main() {
    if let Err(message) = run() {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = parse()?;
    let files = Arc::new(StdFileAccess::new());
    let root = files
        .add_root(&options.folder, None, RootKind::Library)
        .map_err(|e| e.to_string())?;
    let store = SqliteLibraryStore::in_memory().map_err(|e| e.to_string())?;
    let mut last_report = Instant::now();
    let mut progress = |p: ScanProgress| {
        if last_report.elapsed() > Duration::from_millis(250) {
            last_report = Instant::now();
            eprint!(
                "\rscanning: {:?} {} folders, {} files   ",
                p.phase, p.folders_listed, p.files_found
            );
        }
    };
    let scan = block_on(scan_root(
        &*files,
        &store,
        &root.id,
        None,
        Some(&mut progress),
    ))
    .map_err(|e| e.to_string())?;
    eprint!("\r{:60}\r", "");

    match options.command.as_str() {
        "scan" => {
            println!(
                "{} playlists, {} tracks, {} unresolved entries",
                scan.playlists.len(),
                scan.tracks.len(),
                scan.unresolved_entries.len()
            );
            for (i, playlist) in scan.playlists.iter().enumerate() {
                println!(
                    "  [{i}] {} ({} tracks)",
                    playlist.name,
                    playlist.track_file_ids.len()
                );
            }
            Ok(())
        }
        "play" => play(
            files,
            &scan,
            pick_playlist(&scan, options.playlist.as_deref())?,
            &options,
        ),
        "render" => render(
            files,
            &scan,
            pick_playlist(&scan, options.playlist.as_deref())?,
            &options,
        ),
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

/// The playable track ids of the chosen playlist: by index or by (case-insensitive) name, or the first one when none is given.
fn pick_playlist(scan: &ScanResult, choice: Option<&str>) -> Result<Vec<String>, String> {
    let playlist = match choice {
        None => scan.playlists.first(),
        Some(c) => c
            .parse::<usize>()
            .ok()
            .and_then(|i| scan.playlists.get(i))
            .or_else(|| {
                scan.playlists
                    .iter()
                    .find(|p| p.name.eq_ignore_ascii_case(c))
            }),
    }
    .ok_or("no such playlist (run `beatseam scan <folder>` to list them)")?;
    let ids: Vec<String> = playlist
        .track_file_ids
        .iter()
        .filter(|id| scan.tracks.iter().any(|t| &t.file_id == *id && !t.missing))
        .cloned()
        .collect();
    if ids.is_empty() {
        return Err(format!("playlist {} has no playable tracks", playlist.name));
    }
    println!("playlist: {} ({} tracks)", playlist.name, ids.len());
    Ok(ids)
}

fn file_ref(track: &TrackRecord) -> FileRef {
    let name = track
        .relative_path
        .rsplit('/')
        .next()
        .unwrap_or(&track.relative_path)
        .to_owned();
    FileRef {
        id: track.file_id.clone(),
        name,
        relative_path: track.relative_path.clone(),
        size_bytes: track.size_bytes,
        last_modified_ms: track.last_modified_ms,
    }
}

struct Session {
    runtime: Arc<CliRuntime>,
    engine: Rc<MixerEngine<StdFileAccess>>,
    player: PlaylistPlayer<MixerEngine<StdFileAccess>, CliRuntime>,
    names: HashMap<String, String>,
}

fn session(
    files: Arc<StdFileAccess>,
    scan: &ScanResult,
    ids: Vec<String>,
    options: &Options,
    rate: u32,
) -> Session {
    let by_id: HashMap<String, FileRef> = scan
        .tracks
        .iter()
        .map(|t| (t.file_id.clone(), file_ref(t)))
        .collect();
    let names = by_id
        .iter()
        .map(|(id, f)| (id.clone(), f.relative_path.clone()))
        .collect();
    let engine = Rc::new(MixerEngine::new(files, rate));
    let runtime = CliRuntime::new();
    let resolve: ResolveTrack = {
        let by_id = Rc::new(by_id);
        Rc::new(move |id: &str| {
            let found = by_id
                .get(id)
                .cloned()
                .ok_or_else(|| format!("unknown track {id}"));
            Box::pin(async move { found })
        })
    };
    let player = PlaylistPlayer::new(
        engine.clone(),
        runtime.clone(),
        resolve,
        PlaylistPlayerOptions {
            crossfade_seconds: Some(options.crossfade),
            on_error: Some(Rc::new(|message, id| {
                eprintln!(
                    "\nplayback error{}: {message}",
                    id.map(|i| format!(" ({i})")).unwrap_or_default()
                )
            })),
            ..Default::default()
        },
    );
    player.set_volume(options.volume);
    if options.shuffle {
        player.set_shuffle(true);
    }
    let p = player.clone();
    runtime.spawn(Box::pin(async move {
        p.set_playlist(ids, None, PlaylistOptions::default()).await
    }));
    runtime.run_until_stalled();
    player.play();
    Session {
        runtime,
        engine,
        player,
        names,
    }
}

/// One turn of the app loop: end-of-track callbacks, the preload scheduler, and anything the futures were waiting on.
fn tick(s: &Session, now_ms: u64) {
    s.runtime.set_now(now_ms);
    s.engine.poll_events();
    s.player.check_preload();
    s.runtime.run_until_stalled();
}

fn status_line(s: &Session) -> String {
    let state = s.player.get_state();
    let name = state
        .current_file_id
        .as_deref()
        .and_then(|id| s.names.get(id))
        .map(String::as_str)
        .unwrap_or("-");
    let fading = if state.track.pending_incoming.is_some() {
        "  [crossfading]"
    } else {
        ""
    };
    format!(
        "{}/{}  {:>6.1}/{:.1}s  {name}{fading}",
        state.position + 1,
        state.total_tracks,
        state.track.position_seconds,
        state.track.duration_seconds
    )
}

/// The last track has ended on its own (nothing is loading, fading or left to advance to).
fn finished(s: &Session) -> bool {
    let state = s.player.get_state();
    state.track.status == TrackPlayerStatus::Stopped
        && state.track.pending_incoming.is_none()
        && state.position >= state.total_tracks as i64 - 1
}

fn play(
    files: Arc<StdFileAccess>,
    scan: &ScanResult,
    ids: Vec<String>,
    options: &Options,
) -> Result<(), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!(
            "only f32 output is handled, the device gives {:?}",
            config.sample_format()
        ));
    }
    let (rate, channels) = (config.sample_rate(), config.channels() as usize);
    println!(
        "output: {} | {rate} Hz, {channels} ch",
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_default()
    );

    let s = session(files, scan, ids, options, rate);
    let core = s.engine.core();
    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [f32], _| {
                core.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .render(data, channels)
            },
            |e| eprintln!("stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;

    let started = Instant::now();
    let mut last_status: Option<Instant> = None;
    loop {
        tick(&s, started.elapsed().as_millis() as u64);
        if last_status.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
            last_status = Some(Instant::now());
            eprint!("\r{:100}\r{}", "", status_line(&s));
        }
        if finished(&s)
            || options
                .secs
                .is_some_and(|limit| started.elapsed().as_secs_f64() >= limit)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(300)); // let the device buffer drain
    eprintln!();
    Ok(())
}

fn render(
    files: Arc<StdFileAccess>,
    scan: &ScanResult,
    ids: Vec<String>,
    options: &Options,
) -> Result<(), String> {
    const RATE: u32 = 48_000;
    let out_path = options
        .out
        .as_deref()
        .ok_or("render needs --out file.wav")?;
    let s = session(files, scan, ids, options, RATE);
    let limit_frames = options.secs.map(|secs| (secs * RATE as f64) as u64);
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(out_path, spec).map_err(|e| e.to_string())?;
    let chunk_frames = (RATE / 10) as usize;
    let mut chunk = vec![0.0f32; chunk_frames * 2];
    let (mut frames, mut peak, mut changes) = (0u64, 0.0f32, 0u32);
    let mut last_current = s.player.get_state().current_file_id;
    loop {
        s.engine.render(&mut chunk, 2);
        frames += chunk_frames as u64;
        for sample in &chunk {
            peak = peak.max(sample.abs());
            writer
                .write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .map_err(|e| e.to_string())?;
        }
        tick(&s, frames * 1000 / u64::from(RATE));
        let current = s.player.get_state().current_file_id;
        if current != last_current {
            changes += 1;
            println!(
                "{:7.1}s  {}",
                frames as f64 / f64::from(RATE),
                status_line(&s)
            );
            last_current = current;
        }
        if finished(&s) || limit_frames.is_some_and(|limit| frames >= limit) {
            break;
        }
        if frames > 6 * 3600 * u64::from(RATE) {
            return Err("rendered six hours without finishing; giving up".into());
        }
    }
    writer.finalize().map_err(|e| e.to_string())?;
    println!(
        "wrote {} ({:.1}s, peak {peak:.3}, {changes} track changes)",
        out_path.display(),
        frames as f64 / f64::from(RATE)
    );
    Ok(())
}
