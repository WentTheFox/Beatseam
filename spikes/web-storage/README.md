# Phase 0 spike: web library storage, SQLite-wasm vs IndexedDB

Decides the open choice in docs/rust-gpui-rewrite-plan.md ("SQLite-wasm on OPFS, or IndexedDB via `idb`").
Same workload on each backend, in a dedicated worker: 20,000 tracks, an index on (artist, title).

## Run

```text
cd <scratch dir>; npm install playwright @sqlite.org/sqlite-wasm     # 3.54.1-build1 was used
PLAYWRIGHT_DIR=<that dir> node run.mjs
```

## Results (headless Chromium on Windows; ms)

| | SQLite-wasm, `opfs-sahpool` | SQLite-wasm, `opfs` | IndexedDB |
| --- | --- | --- | --- |
| Insert 20,000 rows, one transaction | **401** | 2,089 | 619 |
| Page of 50 ordered by artist,title at offset 10,000 | **1.8** | 2.3 | 10.1 |
| Substring search over title and artist (4,003 hits) | **5.6** (`LIKE`) | 6.3 | 555 (full cursor scan in JS) |
| Update 1,000 rows | 282 | 415 | **198** |
| Reopen, then count | 18 | 24 | **0.6** |
| Needs cross-origin isolation headers | no | yes (`SharedArrayBuffer`) | no |

`sqlite3.wasm` is 879 KB (407 KB gzipped).

## Decision

SQLite-wasm with the `opfs-sahpool` VFS. It is the fastest at the workloads the library screen has (ordered paging,
search), it needs no COOP/COEP headers, and it lets web, desktop and Android share one schema and the same SQL (the
`LibraryStore` implementations differ only in the connection). IndexedDB is competitive for bulk writes but has no
query language, so search is a 100x slower full scan and every query would be hand-written cursor code.

Costs to plan for:

* `opfs-sahpool` holds an exclusive lock on its files, so only one connection (one tab) can have the database open at
  a time; a second tab needs a leader election (Web Locks) or must stay read-only/server-backed.
* It must run in a worker, so the store lives behind message passing, and the 407 KB (gzipped) download is paid once
  and cached.
* Not verified: reaching SQLite from the Rust wasm code. The options are calling the JS module through `wasm-bindgen`
  from the worker (what this spike measured), or the `sqlite-wasm-rs` crate; the first needs no extra build tooling.
* Storage quota/eviction of OPFS in real browsers (Chromium here; Safari and Firefox untested).
