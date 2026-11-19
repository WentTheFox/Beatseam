// Generates tests/data/encoding_golden.json from Node itself (TextDecoder and Buffer's base64url, which is what the
// TypeScript encoding.ts was tested against): malformed and well-formed UTF-8 decoded, and base64url encodings.
//
//   node crates/beatseam-api/tools/gen-encoding-golden.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
let seed = 99;
const rnd = () => (seed = (Math.imul(seed, 1103515245) + 12345) & 0x7fffffff);

const inputs = [[0xff], [0xc0, 0x80], [0xe2, 0x82], [0xed, 0xa0, 0x80], [0xf0, 0x9f, 0x98], [0x41, 0x80, 0x42], [0xf8, 0x88, 0x80, 0x80, 0x80], [0xe0, 0x80, 0x80], [0xf4, 0x90, 0x80, 0x80], [0xe2, 0x41], [0xf0, 0x9f, 0x41], [], [0x41]];
for (let n = 0; n < 4000; n++) inputs.push(Array.from({ length: 1 + (n % 11) }, () => rnd() & 0xff)); // mostly malformed
for (const text of ['', 'plain', 'Música — 日本語 😀 𝄞 \u0000 end', 'a:b?c#d%2F e+f/=.mp3']) inputs.push([...Buffer.from(text, 'utf8')]);

const decoder = new TextDecoder();
const cases = inputs.map((bytes) => ({
  hex: Buffer.from(bytes).toString('hex'),
  decoded: decoder.decode(new Uint8Array(bytes)),
  base64url: Buffer.from(bytes).toString('base64url'),
}));
mkdirSync(join(here, '../tests/data'), { recursive: true });
writeFileSync(join(here, '../tests/data/encoding_golden.json'), JSON.stringify(cases));
console.log(`wrote ${cases.length} cases`);
