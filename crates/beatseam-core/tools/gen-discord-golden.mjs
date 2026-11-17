// Generates tests/data/discord_golden.json: the SET_ACTIVITY frames the *TypeScript* buildSetActivityFrame produces (with
// Date.now stubbed so the nonce is reproducible), for tests/discord_golden.rs to compare byte-for-byte.
//
//   node crates/beatseam-core/tools/gen-discord-golden.mjs
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const work = mkdtempSync(join(tmpdir(), 'beatseam-discord-'));
writeFileSync(join(work, 'richPresence.ts'), readFileSync(join(here, '../../../packages/core/src/discord/richPresence.ts'), 'utf8'));
writeFileSync(join(work, 'package.json'), '{"type":"module"}');
const { buildSetActivityFrame } = await import(pathToFileURL(join(work, 'richPresence.ts')).href);

const NOW = 1_700_000_000_123;
Date.now = () => NOW;

const arts = [null, 'https://beatseam.went.tf/art/abc.jpg', 'http://x/y.png', 'data:image/jpeg;base64,abc', 'content://media/1', 'blob:https://x/1', 'httpx-not-a-url', 'HTTP://UPPER', '', 'https://é.example/ü?a=b&c=d'];
const titles = ['Track Title', 'Qu"ote \\ slash', 'line\nbreak\ttab', 'é 日本語 😀', '', '\u0001\u001f\u007f '];
const cases = [{ info: null }];
for (const [i, art] of arts.entries()) {
  cases.push({ info: { title: titles[i % titles.length], artist: titles[(i + 2) % titles.length], startedAtUnixMs: 1_700_000_000_000 + i * 1000, albumArtUrl: art } });
}
cases.push({ info: null }, { info: { title: 'T', artist: 'A', startedAtUnixMs: 0, albumArtUrl: null } });

const frames = cases.map((c) => ({ ...c, frame: buildSetActivityFrame(c.info) }));
mkdirSync(join(here, '../tests/data'), { recursive: true });
writeFileSync(join(here, '../tests/data/discord_golden.json'), JSON.stringify({ now: NOW, frames }));
console.log(`wrote ${frames.length} frames`);
