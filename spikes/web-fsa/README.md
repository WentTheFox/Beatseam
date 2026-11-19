# Phase 0 spike, item 4 (web half): File System Access from Rust

Question (docs/rust-gpui-rewrite-plan.md, Phase 0 item 4): can the `FileAccess` seam on web be written in Rust on
top of the File System Access API through `web-sys`?

Tests a recursive listing with sizes, a byte-range read, text read/write (non-ASCII) and error mapping, all in Rust.
It also restores a directory handle from IndexedDB after a reload and uses it from Rust.

## Run

```text
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130 --locked     # must match the wasm-bindgen crate in Cargo.lock
cd spikes/web-fsa
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/beatseam_spike_web_fsa.wasm
PLAYWRIGHT_DIR=<dir with playwright installed> node run.mjs
```

## Results (headless Chromium on Windows, OPFS directory handle)

| Check | Result |
| --- | --- |
| Seed 361 files via `createWritable` | 736 ms |
| Recursive walk, 361 files, `getFile()` for each size | 123 ms |
| 4-byte range read at offset 1,000,000 of a 3 MB file (`Blob.slice`) | correct, 1.3 ms, file not fully read |
| Non-ASCII text round trip | ok |
| Missing file | rejects with `NotFoundError` (maps cleanly to a `NotFound` variant) |
| Handle stored in IndexedDB, page reloaded, handle used from Rust | works; `queryPermission` = `granted` |
| wasm size | 125 KB (release, `opt-level=z`, no wasm-opt, no app code yet) |

## What this does not prove

* The handle is an OPFS one. A real folder from `showDirectoryPicker()` cannot be driven headlessly. It is the same
  interface, but two things differ and are unverified here: after a reload a picked-folder handle usually reports
  `queryPermission` = `prompt` and `requestPermission()` needs a user gesture (the existing app already handles this,
  the Rust port must too), and I/O on a real disk folder is slower than OPFS, so the 123 ms walk is a floor.
* Chromium only. Firefox and Safari have no `showDirectoryPicker`, same as today's app.

## Gotchas

* The async directory iterator (`values()`) is driven through `js_sys::Reflect`; it is not reliably typed in web-sys.
* `FileSystemDirectoryHandle.removeEntry` with options is `remove_entry_with_options` in web-sys 0.3.107.
* `showDirectoryPicker` is not in stable web-sys (it needs `--cfg=web_sys_unstable_apis`), so the picker call stays a
  few lines of JS in the shell and Rust receives the handle.
