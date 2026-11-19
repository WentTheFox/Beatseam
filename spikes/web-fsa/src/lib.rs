//! Phase 0 spike, item 4 (web half) of docs/rust-gpui-rewrite-plan.md: can the real `FileAccess` seam
//! (list a folder tree, read a byte range, read/write text) be implemented in Rust on the File System
//! Access API through `web-sys`? The handle comes from JS (`showDirectoryPicker()` needs a user gesture and
//! a picker, and a handle survives reloads only via IndexedDB); the I/O is all Rust.

use js_sys::{Function, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    File, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions, FileSystemGetFileOptions,
    FileSystemRemoveOptions, FileSystemWritableFileStream,
};

type Res<T> = Result<T, JsValue>;

pub struct Entry {
    pub path: String,
    pub size: f64,
}

fn now() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

/// `for await (const h of dir.values())`, written against the JS protocol because the async iterator is
/// not typed in every web-sys release.
async fn children(dir: &FileSystemDirectoryHandle) -> Res<Vec<JsValue>> {
    let values = Reflect::get(dir, &"values".into())?.dyn_into::<Function>()?;
    let iter = values.call0(dir)?;
    let next = Reflect::get(&iter, &"next".into())?.dyn_into::<Function>()?;
    let mut out = Vec::new();
    loop {
        let step = JsFuture::from(js_sys::Promise::resolve(&next.call0(&iter)?)).await?;
        if Reflect::get(&step, &"done".into())?.as_bool().unwrap_or(true) {
            return Ok(out);
        }
        out.push(Reflect::get(&step, &"value".into())?);
    }
}

/// Recursive listing: every file under `dir` with its size.
pub async fn walk(dir: &FileSystemDirectoryHandle, prefix: &str, out: &mut Vec<Entry>) -> Res<()> {
    for child in children(dir).await? {
        let kind = Reflect::get(&child, &"kind".into())?.as_string().unwrap_or_default();
        let name = Reflect::get(&child, &"name".into())?.as_string().unwrap_or_default();
        let path = format!("{prefix}{name}");
        if kind == "directory" {
            Box::pin(walk(&child.unchecked_into(), &format!("{path}/"), out)).await?;
        } else {
            let fh: FileSystemFileHandle = child.unchecked_into();
            let file: File = JsFuture::from(fh.get_file()).await?.unchecked_into();
            out.push(Entry { path, size: file.size() });
        }
    }
    Ok(())
}

async fn dir_at(root: &FileSystemDirectoryHandle, path: &str, create: bool) -> Res<FileSystemDirectoryHandle> {
    let mut cur = root.clone();
    let opts = FileSystemGetDirectoryOptions::new();
    opts.set_create(create);
    for part in path.split('/').filter(|p| !p.is_empty()) {
        cur = JsFuture::from(cur.get_directory_handle_with_options(part, &opts)).await?.unchecked_into();
    }
    Ok(cur)
}

async fn file_at(root: &FileSystemDirectoryHandle, path: &str, create: bool) -> Res<FileSystemFileHandle> {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let dir = dir_at(root, dir, create).await?;
    let opts = FileSystemGetFileOptions::new();
    opts.set_create(create);
    Ok(JsFuture::from(dir.get_file_handle_with_options(name, &opts)).await?.unchecked_into())
}

/// Read `len` bytes at `offset` without loading the whole file (what the player's range reads need).
pub async fn read_range(root: &FileSystemDirectoryHandle, path: &str, offset: f64, len: f64) -> Res<Vec<u8>> {
    let file: File = JsFuture::from(file_at(root, path, false).await?.get_file()).await?.unchecked_into();
    let slice = file.slice_with_f64_and_f64(offset, offset + len)?;
    let buf = JsFuture::from(slice.array_buffer()).await?;
    Ok(Uint8Array::new(&buf).to_vec())
}

pub async fn write_bytes(root: &FileSystemDirectoryHandle, path: &str, data: &[u8]) -> Res<()> {
    let fh = file_at(root, path, true).await?;
    let stream: FileSystemWritableFileStream = JsFuture::from(fh.create_writable()).await?.unchecked_into();
    JsFuture::from(stream.write_with_u8_array(data)?).await?;
    JsFuture::from(stream.close()).await?;
    Ok(())
}

pub async fn read_text(root: &FileSystemDirectoryHandle, path: &str) -> Res<String> {
    let file: File = JsFuture::from(file_at(root, path, false).await?.get_file()).await?.unchecked_into();
    Ok(JsFuture::from(file.text()).await?.as_string().unwrap_or_default())
}

async fn remove_all(root: &FileSystemDirectoryHandle, name: &str) -> Res<()> {
    let opts = FileSystemRemoveOptions::new();
    opts.set_recursive(true);
    JsFuture::from(root.remove_entry_with_options(name, &opts)).await?;
    Ok(())
}

/// Browser test entry point. `root` is a directory handle from JS: in CI an OPFS directory (the same
/// interface a picked folder implements), for real use `showDirectoryPicker()`.
#[wasm_bindgen]
pub async fn run_spike(root: FileSystemDirectoryHandle) -> Res<String> {
    let mut report = Vec::new();
    // Seed a library-shaped tree: 30 artists x 3 albums x 4 tracks = 360 files, plus a playlist file.
    let t = now();
    let body = vec![0xA5u8; 4096];
    for a in 0..30 {
        for al in 0..3 {
            for tr in 0..4 {
                write_bytes(&root, &format!("lib/Artist {a}/Album {al}/{tr:02} Track.mp3"), &body).await?;
            }
        }
    }
    write_bytes(&root, "lib/Mix.m3u8", b"#EXTM3U\nArtist 0/Album 0/00 Track.mp3\n").await?;
    report.push(format!("seed 361 files: {:.0} ms", now() - t));

    let t = now();
    let mut entries = Vec::new();
    walk(&dir_at(&root, "lib", false).await?, "", &mut entries).await?;
    let total: f64 = entries.iter().map(|e| e.size).sum();
    report.push(format!("walk: {} files, {} bytes, {:.0} ms", entries.len(), total, now() - t));

    // Byte-range read of a 3 MB file: marker bytes at a known offset.
    let mut big = vec![0u8; 3 * 1024 * 1024];
    big[1_000_000..1_000_004].copy_from_slice(b"BEAT");
    write_bytes(&root, "lib/big.bin", &big).await?;
    let t = now();
    let got = read_range(&root, "lib/big.bin", 1_000_000.0, 4.0).await?;
    report.push(format!(
        "range read of 4 bytes at 1,000,000 of a 3 MB file: {:?} in {:.1} ms (ok={})",
        String::from_utf8_lossy(&got),
        now() - t,
        got == b"BEAT"
    ));

    let sample = "#EXTM3U\n# \u{e9}\u{30b9}\n";
    write_bytes(&root, "lib/edited.m3u8", sample.as_bytes()).await?;
    let text = read_text(&root, "lib/edited.m3u8").await?;
    report.push(format!("text round trip (non-ASCII): ok={}", text == sample));

    // Error shape for a missing file, which the FileAccess trait has to map.
    match read_text(&root, "lib/nope.txt").await {
        Err(e) => report.push(format!(
            "missing file error name: {}",
            Reflect::get(&e, &"name".into()).ok().and_then(|n| n.as_string()).unwrap_or_default()
        )),
        Ok(_) => report.push("missing file: unexpectedly succeeded".into()),
    }

    remove_all(&root, "lib").await?;
    report.push(format!("cleanup: {} entries left in root", children(&root).await?.len()));
    Ok(report.join("\n"))
}

/// Walk an existing handle (used after a reload, with a handle restored from IndexedDB).
#[wasm_bindgen]
pub async fn count_files(root: FileSystemDirectoryHandle) -> Res<f64> {
    let mut entries = Vec::new();
    walk(&root, "", &mut entries).await?;
    Ok(entries.len() as f64)
}

/// Seed `n` small files so a reload test has something to count.
#[wasm_bindgen]
pub async fn seed(root: FileSystemDirectoryHandle, n: u32) -> Res<()> {
    for i in 0..n {
        write_bytes(&root, &format!("seed/f{i}.txt"), b"x").await?;
    }
    Ok(())
}
