// Generates tests/data/duration_golden.json by running the *TypeScript* parseDuration.ts (Node 24 strips the types) over a
// few hundred built and mutated inputs. The Rust port is checked against these results by tests/duration_golden.rs, so a
// divergence from the original (including its "out-of-range byte reads as 0" behaviour) shows up as a failing test.
//
//   node crates/beatseam-core/tools/gen-duration-golden.mjs
//
// Deterministic (seeded), so re-running only changes the output when parseDuration.ts or this script does.
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { durationPrefixBytesNeeded, parseDurationSeconds } from '../../../packages/core/src/metadata/parseDuration.ts';

const here = dirname(fileURLToPath(import.meta.url));

let seed = 0x9e3779b9;
const rnd = () => ((seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 2 ** 32);
const rint = (n) => Math.floor(rnd() * n);

const ascii = (s) => [...s].map((c) => c.charCodeAt(0));
const u32be = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
const u32le = (n) => [n & 255, (n >>> 8) & 255, (n >>> 16) & 255, (n >>> 24) & 255];
const u16le = (n) => [n & 255, (n >>> 8) & 255];

function flac(sampleRate, totalSamples) {
  const packed = (BigInt(sampleRate) << 44n) | (1n << 41n) | (15n << 36n) | BigInt(totalSamples);
  const pb = [];
  for (let i = 7; i >= 0; i--) pb.push(Number((packed >> BigInt(i * 8)) & 0xffn));
  return [...ascii('fLaC'), 0, 0, 0, 0x22, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, ...pb, ...new Array(16).fill(0)];
}

function wav(rate, ch, bits, dataBytes, extra = []) {
  const byteRate = rate * ch * (bits / 8);
  const fmt = [...ascii('fmt '), ...u32le(16), ...u16le(1), ...u16le(ch), ...u32le(rate), ...u32le(byteRate), ...u16le(ch * bits / 8), ...u16le(bits)];
  const data = [...ascii('data'), ...u32le(dataBytes), ...new Array(Math.min(dataBytes, 64)).fill(0)];
  return [...ascii('RIFF'), ...u32le(4 + fmt.length + data.length), ...ascii('WAVE'), ...extra, ...fmt, ...data];
}

function box(type, payload, big = false) {
  return big ? [...u32be(1), ...ascii(type), ...u32be(0), ...u32be(16 + payload.length), ...payload] : [...u32be(8 + payload.length), ...ascii(type), ...payload];
}
function mp4(version, timescale, duration, pre = []) {
  const payload = version === 1
    ? [1, 0, 0, 0, ...new Array(16).fill(0), ...u32be(timescale), ...u32be(Math.floor(duration / 2 ** 32)), ...u32be(duration >>> 0)]
    : [0, 0, 0, 0, ...u32be(0), ...u32be(0), ...u32be(timescale), ...u32be(duration)];
  return [...box('ftyp', ascii('isom')), ...pre, ...box('moov', box('mvhd', payload))];
}

function mp3(opts) {
  const { versionBits = 3, layerBits = 1, bitrateIndex = 9, rateIndex = 0, channelBits = 0, tag = 0, xing = null, vbri = null, padTo = 120 } = opts;
  const b1 = 0xe0 | (versionBits << 3) | (layerBits << 1) | 1;
  const header = [0xff, b1, (bitrateIndex << 4) | (rateIndex << 2), channelBits << 6];
  const mono = channelBits === 3, v1 = versionBits === 3;
  const side = v1 ? (mono ? 17 : 32) : mono ? 9 : 17;
  let body = [];
  if (xing) body = [...new Array(side).fill(0), ...ascii(xing.tag ?? 'Xing'), ...u32be(xing.flags ?? 1), ...u32be(xing.frames)];
  else if (vbri) body = [...new Array(32).fill(0), ...ascii('VBRI'), ...new Array(10).fill(0), ...u32be(vbri.frames)];
  const frame = [...header, ...body];
  while (frame.length < padTo) frame.push(0);
  const id3 = tag > 0 ? [...ascii('ID3'), 3, 0, 0, (tag >> 21) & 127, (tag >> 14) & 127, (tag >> 7) & 127, tag & 127, ...new Array(tag).fill(0)] : [];
  return [...id3, ...frame];
}

const bases = [];
const add = (name, bytes, size) => bases.push({ name, bytes: Uint8Array.from(bytes), size: size ?? bytes.length });

add('a.flac', flac(44100, 44100 * 125));
add('b.FLAC', flac(48000, 48000 * 3600 + 17));
add('zero-rate.flac', flac(0, 1000));
add('c.wav', wav(44100, 2, 16, 44100 * 4 * 10));
add('d.wav', wav(8000, 1, 8, 8000, [...ascii('LIST'), ...u32le(3), 1, 2, 3, 0]));
add('zero.wav', wav(0, 0, 8, 10));
add('e.m4a', mp4(0, 1000, 90000));
add('f.mp4', mp4(1, 44100, 44100 * 200 + 7));
add('g.aac', mp4(0, 0, 100));
add('big-box.m4a', mp4(0, 600, 36000, box('free', [9, 9, 9, 9], true)));
add('no-moov.m4a', [...box('ftyp', ascii('isom')), ...box('free', [0, 0, 0, 0])]);
for (const [i, o] of [
  { xing: { frames: 9999 } },
  { xing: { frames: 9999, tag: 'Info' } },
  { xing: { frames: 7, flags: 0 } },
  { channelBits: 3, xing: { frames: 4321 } },
  { versionBits: 2, rateIndex: 1, bitrateIndex: 8, xing: { frames: 1000 } },
  { versionBits: 0, rateIndex: 2, bitrateIndex: 5, channelBits: 3, xing: { frames: 1000 } },
  { layerBits: 2, bitrateIndex: 7, rateIndex: 2, xing: { frames: 500 } },
  { layerBits: 3, bitrateIndex: 7, xing: { frames: 500 } },
  { versionBits: 2, layerBits: 3, bitrateIndex: 7 },
  { vbri: { frames: 2000 } },
  {},
  { tag: 300, xing: { frames: 1234 } },
  { tag: 5000 },
  { rateIndex: 3 },
  { bitrateIndex: 15 },
  { bitrateIndex: 0 },
].entries()) add(`m${i}.mp3`, mp3(o), o.tag ? undefined : 40000);

const cases = [];
const record = (name, bytes, size) => {
  const r = parseDurationSeconds(bytes, name, size);
  cases.push({ name, size, hex: Buffer.from(bytes).toString('hex'), result: r });
};

for (const b of bases) {
  record(b.name, b.bytes, b.size);
  record(b.name, b.bytes, 0); // total size 0: the MP3 bitrate estimate gives null
  // every truncation of the small ones, so every "cut off mid-field" read is exercised
  if (b.bytes.length <= 200) for (let n = 0; n < b.bytes.length; n++) record(b.name, b.bytes.subarray(0, n), b.size);
  else for (let n = 0; n < 130; n += 3) record(b.name, b.bytes.subarray(0, n), b.size);
  // random byte flips
  for (let k = 0; k < 14; k++) {
    const m = Uint8Array.from(b.bytes.subarray(0, Math.min(b.bytes.length, 400)));
    for (let f = 0; f < 1 + rint(3); f++) m[rint(m.length)] = rint(256);
    record(b.name, m, b.size);
  }
}
// extensions: case, none, unknown, path-like
for (const name of ['x.MP3', 'x.Flac', 'noext', 'a.b.flac', 'x.ogg', '.mp3', 'x.']) record(name, Uint8Array.from(flac(44100, 441000)), 5000);

const prefixCases = [];
for (const [name, tagSize] of [['a.mp3', 0], ['a.mp3', 100], ['a.mp3', 900000], ['a.mp3', 20_000_000], ['a.flac', 900000], ['A.MP3', 600000]]) {
  const head = tagSize > 0 ? Uint8Array.from([...ascii('ID3'), 3, 0, 0, (tagSize >> 21) & 127, (tagSize >> 14) & 127, (tagSize >> 7) & 127, tagSize & 127]) : new Uint8Array(10);
  prefixCases.push({ name, hex: Buffer.from(head).toString('hex'), result: durationPrefixBytesNeeded(head, name) });
}
prefixCases.push({ name: 'a.mp3', hex: '494433', result: durationPrefixBytesNeeded(Uint8Array.from([0x49, 0x44, 0x33]), 'a.mp3') });

writeFileSync(join(here, '../tests/data/duration_golden.json'), JSON.stringify({ cases, prefixCases }));
const nulls = cases.filter((c) => c.result === null).length;
console.log(`wrote ${cases.length} cases (${nulls} null, ${cases.length - nulls} with a duration) and ${prefixCases.length} prefix cases`);
