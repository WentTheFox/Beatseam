//! Phase 0 spike, item 4 (Android half): is plain `std::fs` enough for file access on Android?
//!
//! Today's app does not use the Storage Access Framework at all: `BeatseamFileAccessModule.kt` reads with
//! `java.io.File` under `MANAGE_EXTERNAL_STORAGE` ("All files access"), because a Samsung SAF picker rejected
//! folder grants. If native code under the same permission can use `std::fs` directly, the Rust `FileAccess`
//! on Android needs no JNI. Results go to logcat under the tag `beatseam-spike` (lines start with `fs:`).

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const ROOT: &str = "/storage/emulated/0";

fn walk(dir: &Path, files: &mut Vec<(PathBuf, u64)>, cap: usize) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        if files.len() >= cap {
            return Ok(());
        }
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            // A subfolder we can't read must not abort the whole walk.
            let _ = walk(&entry.path(), files, cap);
        } else {
            files.push((entry.path(), meta.len()));
        }
    }
    Ok(())
}

pub fn run() {
    log::info!("fs: probe starting");
    match fs::read_dir(ROOT) {
        Ok(rd) => {
            let names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            log::info!("fs: read_dir({ROOT}) ok, {} entries: {}", names.len(), names.join(", "));
        }
        Err(e) => log::info!("fs: read_dir({ROOT}) FAILED kind={:?} os={:?} {e}", e.kind(), e.raw_os_error()),
    }

    for dir in ["Music", "Download"] {
        let path = Path::new(ROOT).join(dir);
        let started = Instant::now();
        let mut files = Vec::new();
        match walk(&path, &mut files, 50_000) {
            Ok(()) => {
                let bytes: u64 = files.iter().map(|f| f.1).sum();
                log::info!("fs: walk {} -> {} files, {} MB, {} ms", path.display(), files.len(), bytes / 1_000_000, started.elapsed().as_millis());
                if let Some((p, len)) = files.iter().find(|(p, _)| p.extension().is_some_and(|e| e == "mp3" || e == "flac" || e == "m4a")) {
                    let t = Instant::now();
                    let res = fs::File::open(p).and_then(|mut f| {
                        f.seek(SeekFrom::Start(len / 2))?;
                        let mut buf = vec![0u8; 64 * 1024];
                        let n = f.read(&mut buf)?;
                        Ok(n)
                    });
                    log::info!("fs: range read 64 KiB at middle of {} ({} bytes): {:?} in {} us", p.display(), len, res, t.elapsed().as_micros());
                }
            }
            Err(e) => log::info!("fs: walk {} FAILED kind={:?} os={:?} {e}", path.display(), e.kind(), e.raw_os_error()),
        }
    }

    // Write, read back, delete, in the app's debug-screenshot folder (see CLAUDE.md).
    let dir = Path::new(ROOT).join("Pictures/Screenshots/Beatseam");
    let file = dir.join("fsprobe.txt");
    let res = fs::create_dir_all(&dir)
        .and_then(|_| fs::File::create(&file).and_then(|mut f| f.write_all("#EXTM3U\n\u{e9}\u{30b9}\n".as_bytes())))
        .and_then(|_| fs::read_to_string(&file))
        .and_then(|s| fs::remove_file(&file).map(|_| s));
    log::info!("fs: write/read/delete {}: {:?}", file.display(), res);

    // Other apps' private data stays off limits even with All files access on Android 11+.
    match fs::read_dir(Path::new(ROOT).join("Android/data")) {
        Ok(rd) => log::info!("fs: Android/data readable, {} entries", rd.count()),
        Err(e) => log::info!("fs: Android/data FAILED kind={:?} os={:?}", e.kind(), e.raw_os_error()),
    }
    log::info!("fs: probe done");
}
