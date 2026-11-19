// Generates tests/data/embedded_lyrics_golden.json from the TypeScript readEmbeddedLyrics.ts: ID3v2.2/2.3/2.4 tags built
// with every text encoding, unsynchronisation, extended headers, odd frame orders, truncations and random corruption.
//   node crates/beatseam-core/tools/gen-embedded-lyrics-golden.mjs
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const work = mkdtempSync(join(tmpdir(), 'beatseam-id3-'));
writeFileSync(join(work, 'readEmbeddedLyrics.ts'), readFileSync(join(here, '../../../packages/core/src/metadata/readEmbeddedLyrics.ts'), 'utf8'));
writeFileSync(join(work, 'package.json'), '{"type":"module"}');
const { readEmbeddedLyrics, id3v2TagLength } = await import(pathToFileURL(join(work, 'readEmbeddedLyrics.ts')).href);

let seed = 4242;
const rnd = () => (seed = (Math.imul(seed, 1103515245) + 12345) & 0x7fffffff);
const rint = (n) => rnd() % n;

const ss = (n) => [(n >> 21) & 127, (n >> 14) & 127, (n >> 7) & 127, n & 127];
const u32 = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
const ascii = (s) => [...s].map((c) => c.charCodeAt(0));
const utf8 = (s) => [...Buffer.from(s, 'utf8')];
const utf16 = (s, be, bom) => {
  const out = bom ? (be ? [0xfe, 0xff] : [0xff, 0xfe]) : [];
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    out.push(...(be ? [c >> 8, c & 255] : [c & 255, c >> 8]));
  }
  return out;
};
const latin1 = (s) => [...s].map((c) => c.charCodeAt(0) & 255);

function encode(text, enc) {
  switch (enc) {
    case 1: return utf16(text, false, true);
    case 2: return utf16(text, true, false);
    case 3: return utf8(text);
    default: return latin1(text);
  }
}
const nul = (enc) => (enc === 1 || enc === 2 ? [0, 0] : [0]);

function uslt(enc, descriptor, lyrics) {
  return [enc, ...ascii('eng'), ...encode(descriptor, enc), ...nul(enc), ...encode(lyrics, enc)];
}
function frame(major, id, data, flags = [0, 0]) {
  if (major === 2) return [...ascii(id), (data.length >> 16) & 255, (data.length >> 8) & 255, data.length & 255, ...data];
  return [...ascii(id), ...(major === 4 ? ss(data.length) : u32(data.length)), ...flags, ...data];
}
const unsync = (bytes) => bytes.flatMap((b, i) => (b === 0xff ? [0xff, 0] : [b]));
function tag(major, frames, { flags = 0, ext = [] } = {}) {
  let body = [...ext, ...frames.flat()];
  if (flags & 0x80 && major < 4) body = unsync(body);
  return [...ascii('ID3'), major, 0, flags, ...ss(body.length), ...body];
}

const texts = ['[00:01.00]Hello\n[00:02.00]World', 'plain lyrics', 'héllo wörld ♪', '日本語の歌詞', '😀 emoji', 'with trailing nul\0\0', '', '   ', 'line1\r\nline2'];
const cases = [];
const add = (bytes) => cases.push({ hex: Buffer.from(bytes).toString('hex'), lyrics: readEmbeddedLyrics(Uint8Array.from(bytes)), length: id3v2TagLength(Uint8Array.from(bytes)) });

for (const major of [2, 3, 4]) {
  for (const enc of [0, 1, 2, 3]) {
    for (const text of texts) {
      const id = major === 2 ? 'ULT' : 'USLT';
      const lyricsFrame = frame(major, id, uslt(enc, enc % 2 ? 'd' : '', text));
      add(tag(major, [lyricsFrame]));
      add(tag(major, [frame(major, major === 2 ? 'TT2' : 'TIT2', [0, ...latin1('Title')]), lyricsFrame]));
      add(tag(major, [lyricsFrame], { flags: 0x80 }));
    }
  }
}
// extended headers (v2.3: size excludes itself; v2.4: synchsafe size including itself)
const lf3 = frame(3, 'USLT', uslt(0, '', 'ext header lyrics'));
add(tag(3, [lf3], { flags: 0x40, ext: [...u32(6), 0, 0, 0, 0, 0, 0] }));
const lf4 = frame(4, 'USLT', uslt(3, '', 'ext header lyrics'));
add(tag(4, [lf4], { flags: 0x40, ext: [...ss(6), 1, 0, 0, 0] }));
// padding after frames, unknown version, tiny/garbage heads
add([...tag(3, [lf3]), 0, 0, 0, 0]);
add(ascii('ID3\x05\x00\x00\x00\x00\x00\x00'));
add(ascii('ID3\x01\x00\x00\x00\x00\x00\x00'));
add(ascii('ID3'));
add([]);
add(ascii('not a tag at all, just text'));
// frames whose declared size overruns, USLT shorter than 5 bytes
add(tag(3, [[...ascii('USLT'), ...u32(1000), 0, 0, 0]]));
add(tag(3, [frame(3, 'USLT', [0, 101, 110])]));
// malformed UTF-8 / odd UTF-16
add(tag(3, [frame(3, 'USLT', [3, ...ascii('eng'), 0, 0x41, 0x80, 0xc3, 0x28, 0xf0, 0x9f, 0x98, 0xe2, 0x82])]));
add(tag(3, [frame(3, 'USLT', [1, ...ascii('eng'), 0, 0, 0xff, 0xfe, 0x41, 0x00, 0x42])]));
add(tag(3, [frame(3, 'USLT', [2, ...ascii('eng'), 0, 0, 0xfe, 0xff, 0x00, 0x41, 0x00])]));
// every truncation and random corruption of a few good tags
const base = [tag(3, [frame(3, 'TIT2', [0, ...latin1('T')]), frame(3, 'USLT', uslt(1, 'd', '[00:01.00]Truncate me'))]), tag(4, [frame(4, 'USLT', uslt(3, 'x', 'héllo'))]), tag(2, [frame(2, 'ULT', uslt(0, '', 'v2.2 lyrics'))])];
for (const b of base) {
  for (let n = 0; n < b.length; n++) add(b.slice(0, n));
  for (let k = 0; k < 150; k++) {
    const m = [...b];
    for (let f = 0; f < 1 + rint(4); f++) m[rint(m.length)] = rint(256);
    add(m);
  }
}

mkdirSync(join(here, '../tests/data'), { recursive: true });
writeFileSync(join(here, '../tests/data/embedded_lyrics_golden.json'), JSON.stringify(cases));
console.log(`wrote ${cases.length} cases (${cases.filter((c) => c.lyrics !== null).length} with lyrics)`);
