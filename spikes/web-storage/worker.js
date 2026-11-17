// Phase 0 spike (web storage decision): the same library workload against SQLite-wasm on OPFS and against
// IndexedDB, both inside a dedicated worker (where the real app's store would live).
import sqlite3InitModule from '/sqlite/index.mjs';

const N = 20000;

// Deterministic fake library so every run and every backend sees the same rows.
function rows() {
  let s = 12345;
  const rnd = () => (s = (s * 1664525 + 1013904223) >>> 0) / 2 ** 32;
  const words = ['Mood', 'Aether', 'Dive', 'Technology', 'Enchantress', 'Night', 'Rio', 'Take', 'You', 'Fire', 'Echo', 'Ghost', 'Blue', 'Moon'];
  const w = () => words[Math.floor(rnd() * words.length)];
  return Array.from({ length: N }, (_, i) => ({
    id: i + 1,
    title: `${w()} ${w()} ${i}`,
    artist: `${w()} ${Math.floor(rnd() * 800)}`,
    album: `${w()} ${w()}`,
    path: `Music/${w()}/${w()} ${i}.mp3`,
    duration: 120 + rnd() * 240,
    has_lyrics: rnd() < 0.3 ? 1 : 0,
  }));
}

const time = async (f) => {
  const t = performance.now();
  const r = await f();
  return [Math.round((performance.now() - t) * 10) / 10, r];
};

async function sqlite(vfs) {
  const out = {};
  const sqlite3 = await sqlite3InitModule();
  let open;
  if (vfs === 'sahpool') {
    const pool = await sqlite3.installOpfsSAHPoolVfs({ clearOnInit: true, name: 'beatseam-spike' });
    open = () => new pool.OpfsSAHPoolDb('/lib.db');
  } else {
    // The "opfs" VFS needs SharedArrayBuffer, i.e. cross-origin isolation headers on the page.
    if (!sqlite3.oo1.OpfsDb) throw new Error('opfs VFS unavailable (no SharedArrayBuffer / not isolated)');
    open = () => new sqlite3.oo1.OpfsDb('/lib-opfs.db', 'ct');
  }
  let db;
  [out.open_ms, db] = await time(() => open());
  db.exec(`drop table if exists tracks;
    create table tracks(id integer primary key, title text, artist text, album text, path text, duration real, has_lyrics integer);
    create index tracks_artist_title on tracks(artist, title);`);
  const data = rows();
  [out.insert_ms] = await time(() => {
    db.exec('begin');
    const st = db.prepare('insert into tracks values(?,?,?,?,?,?,?)');
    for (const r of data) st.bind([r.id, r.title, r.artist, r.album, r.path, r.duration, r.has_lyrics]).stepReset();
    st.finalize();
    db.exec('commit');
  });
  [out.page_ms] = await time(() => db.selectArrays('select id,title,artist from tracks order by artist,title limit 50 offset 10000'));
  [out.search_like_ms, out.search_hits] = await time(() => db.selectValue("select count(*) from tracks where title like '%ghost%' or artist like '%ghost%'"));
  [out.count_ms, out.count] = await time(() => db.selectValue('select count(*) from tracks'));
  [out.update_1000_ms] = await time(() => db.exec('update tracks set has_lyrics = 1 - has_lyrics where id % 20 = 0'));
  db.close();
  [out.reopen_ms, db] = await time(() => open());
  [out.count_after_reopen_ms, out.count_after_reopen] = await time(() => db.selectValue('select count(*) from tracks'));
  db.close();
  out.sqlite_version = sqlite3.version.libVersion;
  return out;
}

const idbReq = (r) => new Promise((res, rej) => { r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error); });
const idbDone = (tx) => new Promise((res, rej) => { tx.oncomplete = res; tx.onerror = () => rej(tx.error); tx.onabort = () => rej(tx.error); });

async function idb() {
  const out = {};
  await new Promise((res) => { const d = indexedDB.deleteDatabase('spike-lib'); d.onsuccess = d.onerror = res; });
  const open = () => new Promise((res, rej) => {
    const r = indexedDB.open('spike-lib', 1);
    r.onupgradeneeded = () => {
      const s = r.result.createObjectStore('tracks', { keyPath: 'id' });
      s.createIndex('artist_title', ['artist', 'title']);
    };
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  });
  let db;
  [out.open_ms, db] = await time(open);
  const data = rows();
  [out.insert_ms] = await time(async () => {
    const tx = db.transaction('tracks', 'readwrite');
    const s = tx.objectStore('tracks');
    for (const r of data) s.put(r);
    await idbDone(tx);
  });
  [out.page_ms] = await time(() => new Promise((res, rej) => {
    const c = db.transaction('tracks').objectStore('tracks').index('artist_title').openCursor();
    let first = true, got = 0;
    const page = [];
    c.onsuccess = () => {
      const cur = c.result;
      if (!cur) return res(page);
      if (first) { first = false; cur.advance(10000); return; }
      page.push(cur.value);
      if (++got === 50) return res(page);
      cur.continue();
    };
    c.onerror = () => rej(c.error);
  }));
  // No LIKE: a substring search is a full scan in JS.
  [out.search_like_ms, out.search_hits] = await time(() => new Promise((res, rej) => {
    const c = db.transaction('tracks').objectStore('tracks').openCursor();
    let hits = 0;
    c.onsuccess = () => {
      const cur = c.result;
      if (!cur) return res(hits);
      const v = cur.value;
      if (v.title.toLowerCase().includes('ghost') || v.artist.toLowerCase().includes('ghost')) hits++;
      cur.continue();
    };
    c.onerror = () => rej(c.error);
  }));
  [out.count_ms, out.count] = await time(() => idbReq(db.transaction('tracks').objectStore('tracks').count()));
  [out.update_1000_ms] = await time(async () => {
    const tx = db.transaction('tracks', 'readwrite');
    const s = tx.objectStore('tracks');
    for (let id = 20; id <= N; id += 20) {
      const v = await idbReq(s.get(id));
      v.has_lyrics = 1 - v.has_lyrics;
      s.put(v);
    }
    await idbDone(tx);
  });
  db.close();
  [out.reopen_ms, db] = await time(open);
  [out.count_after_reopen_ms, out.count_after_reopen] = await time(() => idbReq(db.transaction('tracks').objectStore('tracks').count()));
  db.close();
  return out;
}

self.onmessage = async (e) => {
  try {
    const r = e.data === 'idb' ? await idb() : await sqlite(e.data);
    self.postMessage({ ok: true, result: r });
  } catch (err) {
    self.postMessage({ ok: false, error: String(err && err.stack ? err.stack : err) });
  }
};
