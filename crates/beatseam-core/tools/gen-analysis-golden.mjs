// Generates tests/data/analysis_golden.json by running the *TypeScript* analysis code (bpm, loudness, mono, silence,
// analyzeTrack) over synthetic signals described by small "recipes" that tests/analysis_golden.rs rebuilds with the same
// arithmetic. The Rust port is checked against these results.
//
//   node crates/beatseam-core/tools/gen-analysis-golden.mjs
//
// The TS sources use extensionless relative imports, which Node's type stripping can't resolve, so they are copied to a
// temp directory with `.ts` added to those imports first (the sources themselves are not touched).
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const srcDir = join(here, '../../../packages/core/src/analysis');
const work = mkdtempSync(join(tmpdir(), 'beatseam-analysis-'));
for (const f of ['bpm', 'loudness', 'mono', 'silence', 'analyzeTrack', 'yieldToEventLoop']) {
  const text = readFileSync(join(srcDir, `${f}.ts`), 'utf8').replace(/from '(\.\/[A-Za-z]+)'/g, "from '$1.ts'");
  writeFileSync(join(work, `${f}.ts`), text);
}
writeFileSync(join(work, 'package.json'), '{"type":"module"}');
const load = (f) => import(pathToFileURL(join(work, `${f}.ts`)).href);
const { estimateBpm } = await load('bpm');
const { measureLoudnessDb, computeNormalizationGain } = await load('loudness');
const { mixToMono } = await load('mono');
const { findContentBounds } = await load('silence');
const { analyzeTrack } = await load('analyzeTrack');

// ---- signal recipes (mirrored by tests/analysis_golden.rs) ----
function build(r) {
  switch (r.k) {
    case 'silence':
      return new Float32Array(r.n);
    case 'square': {
      const out = new Float32Array(r.n);
      for (let i = 0; i < r.n; i++) out[i] = r.amp * (Math.floor((i * 2 * r.hz) / r.sr) % 2 === 0 ? 1 : -1);
      return out;
    }
    case 'noise': {
      const out = new Float32Array(r.n);
      let s = r.seed >>> 0;
      for (let i = 0; i < r.n; i++) {
        s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
        out[i] = (s / 2 ** 32 * 2 - 1) * r.amp;
      }
      return out;
    }
    case 'clicks': {
      const length = Math.round(r.dur * r.sr);
      const out = new Float32Array(length);
      const period = 60 / r.bpm;
      for (let beat = r.first; beat < r.dur; beat += period) {
        const start = Math.round(beat * r.sr);
        const end = Math.min(length, start + Math.round(0.05 * r.sr));
        for (let i = start; i < end; i++) out[i] = r.amp * (Math.floor(((i - start) * 2 * 1000) / r.sr) % 2 === 0 ? 1 : -1);
      }
      return out;
    }
    case 'concat': {
      const parts = r.parts.map(build);
      const out = new Float32Array(parts.reduce((s, p) => s + p.length, 0));
      let o = 0;
      for (const p of parts) (out.set(p, o), (o += p.length));
      return out;
    }
    default:
      throw new Error(`unknown recipe ${r.k}`);
  }
}

const sil = (n) => ({ k: 'silence', n });
const sq = (n, amp, hz, sr) => ({ k: 'square', n, amp, hz, sr });
const cat = (...parts) => ({ k: 'concat', parts });

const cases = [];
const add = (name, sr, channels) => cases.push({ name, sr, channels });

for (const bpm of [60, 70, 90, 100, 120, 128, 140, 174, 199]) {
  for (const first of [0, 0.2]) add(`clicks ${bpm} bpm first ${first}`, 44100, [{ k: 'clicks', sr: 44100, dur: 12, bpm, first, amp: 0.8 }]);
}
add('clicks 48k', 48000, [{ k: 'clicks', sr: 48000, dur: 10, bpm: 126, first: 0.1, amp: 0.5 }]);
add('clicks 22.05k', 22050, [{ k: 'clicks', sr: 22050, dur: 10, bpm: 111, first: 0.05, amp: 0.5 }]);
for (const seed of [1, 7, 99]) add(`noise ${seed}`, 44100, [{ k: 'noise', n: 200_000, seed, amp: 0.3 }]);
add('silence 5s', 44100, [sil(220500)]);
for (const n of [0, 1, 100, 881, 882, 883, 1023, 1024, 1025, 1536, 2048, 5000]) add(`short ${n}`, 44100, [sq(n, 0.4, 220, 44100)]);
add('tight tone', 44100, [sq(132300, 0.5, 440, 44100)]);
add('padded tone', 44100, [cat(sil(88200), sq(132300, 0.5, 440, 44100), sil(88200))]);
add('padded tone 8k', 8000, [cat(sil(16000), sq(24000, 0.5, 220, 8000), sil(16000))]);
add('quiet below threshold', 44100, [sq(100000, 0.004, 300, 44100)]);
add('quiet just above threshold', 44100, [sq(100000, 0.0058, 300, 44100)]);
add('leading only', 44100, [cat(sil(50000), sq(60000, 0.3, 300, 44100))]);
add('trailing only', 44100, [cat(sq(60000, 0.3, 300, 44100), sil(50000))]);
add('mid gap', 44100, [cat(sq(30000, 0.3, 300, 44100), sil(80000), sq(30000, 0.3, 300, 44100))]);
add('odd window sr 11025', 11025, [cat(sil(7777), sq(20000, 0.2, 200, 11025), sil(3333))]);
add('stereo one silent', 44100, [sil(220500), cat(sil(88200), sq(44100, 0.5, 440, 44100), sil(88200))]);
add('stereo both', 44100, [sq(100000, 0.4, 300, 44100), sq(100000, 0.2, 500, 44100)]);
add('three channels', 22050, [sq(60000, 0.3, 100, 22050), sq(60000, 0.3, 150, 22050), noise(60000)]);
function noise(n) { return { k: 'noise', n, seed: 5, amp: 0.2 }; }
add('long 100s', 8000, [cat(sq(240000, 0.3, 200, 8000), sq(320000, 0.01, 200, 8000), sq(240000, 0.3, 200, 8000))]);
add('long 100s padded', 8000, [cat(sil(80000), sq(240000, 0.3, 200, 8000), sq(320000, 0.01, 200, 8000), sq(240000, 0.3, 200, 8000), sil(80000))]);
add('exactly 30s each side', 8000, [cat(sq(240000, 0.3, 200, 8000), sq(240000, 0.05, 200, 8000))]);
add('just over 60s', 8000, [cat(sq(240000, 0.3, 200, 8000), sq(240001, 0.05, 200, 8000))]);

const out = [];
for (const c of cases) {
  const channels = c.channels.map(build);
  const audio = { sampleRate: c.sr, numberOfChannels: channels.length, channelData: channels, durationSeconds: channels[0].length / c.sr };
  const mono = await mixToMono(audio);
  const bounds = await findContentBounds(audio);
  const boundsGiven = await findContentBounds(audio, mono);
  let sum = 0;
  for (const v of mono) sum += v;
  const bpm = estimateBpm(mono, c.sr);
  const loud = measureLoudnessDb(mono);
  out.push({
    ...c,
    monoLen: mono.length,
    monoSum: sum,
    monoMid: mono.length ? mono[mono.length >> 1] : 0,
    bpm,
    loudnessDb: loud,
    normalizationGainFromLoudness: computeNormalizationGain(loud),
    bounds,
    boundsGiven,
    analysisGain: (await analyzeTrack(audio)).normalizationGain,
  });
}

mkdirSync(join(here, '../tests/data'), { recursive: true });
writeFileSync(join(here, '../tests/data/analysis_golden.json'), JSON.stringify(out));
console.log(`wrote ${out.length} cases`);
