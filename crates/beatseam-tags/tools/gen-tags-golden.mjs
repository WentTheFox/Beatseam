// Runs the *TypeScript* readTags (jsmediatags) over a sample of real audio files and writes what it read, for
// tests/jsmediatags_comparison.rs to hold the Rust reader to. The audio files stay where they are (nothing is copied into
// the repo); the golden file records their paths, so it is for local use only and is not committed.
//
//   node --experimental-transform-types crates/beatseam-tags/tools/gen-tags-golden.mjs <music dir> <out.json> [max files]
import { createHash } from 'node:crypto';
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const [, , musicDir, outFile, maxArg] = process.argv;
if (!musicDir || !outFile) throw new Error('usage: gen-tags-golden.mjs <music dir> <out.json> [max files]');
const max = Number(maxArg ?? 200);

// readTags.ts imports jsmediatags by package name, so it has to live under packages/core to resolve it.
const coreDir = resolve(here, '../../../packages/core');
const work = join(coreDir, '.tags-golden-tmp');
rmSync(work, { recursive: true, force: true });
mkdirSync(work, { recursive: true });
for (const f of ['readTags.ts', 'types.ts', 'jsmediatags.d.ts']) {
  writeFileSync(
    join(work, f),
    readFileSync(join(coreDir, 'src/metadata', f), 'utf8')
      .replace(/from '(\.\/[A-Za-z]+)'/g, "from '$1.ts'")
      // jsmediatags is a CommonJS bundle: Node's ESM loader exposes it as a default export, not named ones.
      .replace("import { Reader, type PictureType } from 'jsmediatags/dist/jsmediatags.min.js';", "import jsmediatags from 'jsmediatags/dist/jsmediatags.min.js';\nconst { Reader } = jsmediatags as { Reader: new (location: number[] | string) => any };\ntype PictureType = { format: string; data: number[] };"),
  );
}
writeFileSync(join(work, 'package.json'), '{"type":"module"}');

function* walk(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full);
    else if (['.mp3', '.flac', '.m4a'].includes(extname(entry.name).toLowerCase())) yield full;
  }
}

try {
  const { readTags } = await import(pathToFileURL(join(work, 'readTags.ts')).href);
  const files = [...walk(musicDir)];
  // An even spread across the library rather than the first N.
  const step = Math.max(1, Math.floor(files.length / max));
  const sample = files.filter((_, i) => i % step === 0).slice(0, max);
  const cases = [];
  for (const path of sample) {
    const bytes = readFileSync(path);
    const buffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    const tags = await readTags(buffer);
    cases.push({
      path,
      size: statSync(path).size,
      tags: tags && {
        title: tags.title,
        artists: tags.artists,
        album: tags.album,
        cover: tags.coverArt && { mimeType: tags.coverArt.mimeType, length: tags.coverArt.data.length, sha256: createHash('sha256').update(tags.coverArt.data).digest('hex') },
      },
    });
  }
  writeFileSync(outFile, JSON.stringify(cases));
  const tagged = cases.filter((c) => c.tags).length;
  console.log(`${cases.length} files from ${files.length} (${tagged} with tags, ${cases.filter((c) => c.tags?.cover).length} with cover art) -> ${outFile}`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
