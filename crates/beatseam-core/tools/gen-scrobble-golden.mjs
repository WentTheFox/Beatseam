// Generates tests/data/scrobble_golden.json by running the *TypeScript* md5, Last.fm client and scrobble queue code:
//   - md5 digests of many strings (unicode, padding-boundary lengths),
//   - the exact HTTP requests (method, url, headers, body) each Last.fm call builds, captured through a fake fetch,
//   - encodeScrobbleQueue output and decodeScrobbleQueue results for assorted persisted values.
// tests/scrobble_golden.rs replays them against the Rust port.
//
//   node --experimental-transform-types crates/beatseam-core/tools/gen-scrobble-golden.mjs   (lastfm.ts uses a TS parameter property, which plain type stripping cannot erase)
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const srcDir = join(here, '../../../packages/core/src/scrobble');
const work = mkdtempSync(join(tmpdir(), 'beatseam-scrobble-'));
for (const f of ['md5', 'lastfm', 'scrobbleQueue']) {
  writeFileSync(join(work, `${f}.ts`), readFileSync(join(srcDir, `${f}.ts`), 'utf8').replace(/from '(\.\/[A-Za-z0-9]+)'/g, "from '$1.ts'"));
}
writeFileSync(join(work, 'package.json'), '{"type":"module"}');
const load = (f) => import(pathToFileURL(join(work, `${f}.ts`)).href);
const { md5 } = await load('md5');
const lastfm = await load('lastfm');
const queue = await load('scrobbleQueue');

let seed = 12345;
const rnd = () => ((seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 2 ** 32);
const rint = (n) => Math.floor(rnd() * n);

// ---- md5 ----
const md5Inputs = ['', 'a', 'abc', 'héllo', '日本語のテキスト', 'emoji 😀🎵', 'é combining', 'tab\tnewline\n', '\u0000nul', 'ÿĀ߿ࠀ￿'];
for (let n = 0; n <= 130; n++) md5Inputs.push('x'.repeat(n));
const alphabet = 'abcdefghijklmnopqrstuvwxyzABC 0123456789&=+%éüß日本😀';
for (let i = 0; i < 60; i++) md5Inputs.push(Array.from({ length: rint(200) }, () => [...alphabet][rint([...alphabet].length)]).join(''));
const md5Cases = md5Inputs.map((input) => ({ input, md5: md5(input) }));

// ---- Last.fm requests ----
const creds = (calls) => ({
  apiKey: 'test key&1',
  apiSecret: 'sécret',
  fetch: async (url, init) => {
    calls.push({ method: init?.method ?? 'GET', url, headers: init?.headers ? Object.entries(init.headers) : [], body: init?.body ?? null });
    return { ok: true, status: 200, json: async () => ({ token: 'tok', session: { key: 'sk', name: 'user' } }) };
  },
});
const tricky = ['Plain', 'A & B', 'a=b', 'sp ace', 'Ünïcödé ☃', '~!\'()*', '100% [x]', 'emoji 😀', '日本語', '', 'new\nline', 'quote"s', 'p+lus'];
const lastfmCases = [];
async function record(call, args, run) {
  const calls = [];
  await run(creds(calls));
  lastfmCases.push({ call, args, requests: calls });
}
await record('getAuthToken', {}, (c) => lastfm.getAuthToken(c));
for (const token of tricky.slice(0, 6)) await record('getSession', { token }, (c) => lastfm.getSession(c, token));
for (const [i, t] of tricky.entries()) {
  const entry = { artist: t, track: tricky[(i + 3) % tricky.length], album: i % 3 === 0 ? null : tricky[(i + 5) % tricky.length], durationSeconds: [null, 0, 199.5, 0.4, 200, 0.5, 1e-7, 3599.999][i % 8] };
  await record('updateNowPlaying', { sessionKey: tricky[(i + 1) % 4], ...entry }, (c) => lastfm.updateNowPlaying(c, tricky[(i + 1) % 4], entry));
}
const entryFor = (i) => ({
  artist: tricky[i % tricky.length],
  track: tricky[(i * 3 + 1) % tricky.length],
  timestamp: 1_700_000_000 + i * 37,
  album: i % 2 ? tricky[(i + 2) % tricky.length] : null,
  durationSeconds: [null, 0, 180, 201.7, 59.5][i % 5],
});
for (const n of [1, 2, 3, 13, 50]) {
  const entries = Array.from({ length: n }, (_, i) => entryFor(i));
  await record('scrobbleBatch', { sessionKey: 'sk/1', entries }, (c) => lastfm.scrobbleBatch(c, 'sk/1', entries));
}
const urls = [];
for (const t of tricky) {
  const c = creds([]);
  urls.push({ token: t, auth: lastfm.buildAuthUrl(c, t), web: lastfm.buildWebAuthUrl(c, `https://x.example/cb?t=${t}&u=1`) });
}

// ---- scrobble queue ----
const queueEncode = [];
const entrySets = [[], [entryFor(0)], Array.from({ length: 6 }, (_, i) => entryFor(i))];
entrySets.push([{ artist: 'ctl \u0000\u001f\u007f   ', track: 'q"b\\s/', timestamp: 0, album: 'é😀', durationSeconds: 0.1 + 0.2 }]);
for (const d of [1e-7, 1.5e-7, 123456789.123, 1e21, 1.5e22, 0.000001, 0.0000001, 100, 5e-324, 2 ** 53, 1 / 3]) entrySets.push([{ artist: 'a', track: 't', timestamp: 1, album: null, durationSeconds: d }]);
for (const entries of entrySets) queueEncode.push({ entries, encoded: queue.encodeScrobbleQueue(entries) });
const decodeRaws = [
  null, '', 'not json', '{"a":1}', '[]', '3', 'null', '[1,2]',
  '[{"artist":"A","track":"T","timestamp":5,"album":null,"durationSeconds":null}]',
  '[{"artist":"A","track":"T","timestamp":5,"album":"X","durationSeconds":12.5},{"artist":1},{"artist":"B","track":"U","timestamp":"6","album":null,"durationSeconds":null}]',
  '[{"artist":"A","track":"T","timestamp":5}]', '[{"artist":"A","track":"T","timestamp":5,"album":null}]',
  '[{"artist":"A","track":"T","timestamp":5,"album":1,"durationSeconds":null}]',
  '[{"artist":"A","track":"T","timestamp":5,"album":null,"durationSeconds":"9"}]',
  '[{"artist":"A","track":"T","timestamp":5,"album":null,"durationSeconds":null,"extra":true}]',
  '[null,{"artist":"A","track":"T","timestamp":5,"album":null,"durationSeconds":null}]',
  queue.encodeScrobbleQueue(entrySets[2]),
];
const queueDecode = decodeRaws.map((raw) => ({ raw, entries: queue.decodeScrobbleQueue(raw) }));

mkdirSync(join(here, '../tests/data'), { recursive: true });
writeFileSync(join(here, '../tests/data/scrobble_golden.json'), JSON.stringify({ md5Cases, lastfmCases, urls, queueEncode, queueDecode }));
console.log(`md5 ${md5Cases.length}, lastfm ${lastfmCases.length}, urls ${urls.length}, queue encode ${queueEncode.length}, decode ${queueDecode.length}`);
