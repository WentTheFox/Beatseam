// Generates tests/data/appupdate_golden.json from the *TypeScript* checkForUpdate.ts: which (latest tag, current version)
// pairs count as "an update is available", and what parseChecksumsFile returns for assorted checksum files.
// tests/appupdate_golden.rs replays them against the Rust port.
//
//   node crates/beatseam-core/tools/gen-appupdate-golden.mjs
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const srcDir = join(here, '../../../packages/core/src/appUpdate');
const work = mkdtempSync(join(tmpdir(), 'beatseam-appupdate-'));
for (const f of ['checkForUpdate', 'githubRepo']) {
  writeFileSync(join(work, `${f}.ts`), readFileSync(join(srcDir, `${f}.ts`), 'utf8').replace(/from '(\.\/[A-Za-z]+)'/g, "from '$1.ts'"));
}
writeFileSync(join(work, 'package.json'), '{"type":"module"}');
const { checkForUpdate, parseChecksumsFile } = await import(pathToFileURL(join(work, 'checkForUpdate.ts')).href);

const versions = [
  'v0.1.0', 'v0.2.0', '0.2.0', 'v1.0.0', 'v0.10.0', 'v0.9.9', 'v10.0.0', 'v1.2', 'v1.2.3.4', 'v0.1.0-3-gabc1234', 'v0.2.0-rc1', '', ' v0.3.0 ',
  'vv0.3.0', 'V0.3.0', 'v0.03.0', 'v00.0.1', 'v1.2.٣', ' v0.3.0 ', '﻿v0.3.0', 'v0.3.0\u0085', 'v99999999999999999999.0.0', 'v+1.0.0',
];
const updateCases = [];
for (const tag of versions) {
  for (const current of ['v0.1.0', 'v0.2.0', 'v0.2.0-5-gdeadbee', 'v1.0.0', '0.3.0', '']) {
    globalThis.fetch = async () => ({
      ok: true,
      status: 200,
      json: async () => ({ tag_name: tag, html_url: 'https://example.test/r', assets: [{ name: 'app.apk', browser_download_url: 'https://example.test/app.apk' }] }),
    });
    let result;
    try {
      result = (await checkForUpdate(current)) !== null;
    } catch {
      result = 'error';
    }
    updateCases.push({ tag, current, result });
  }
}

const H1 = 'a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2';
const H2 = 'FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF';
const texts = [
  `${H1}  app-release.apk\n${H2}  other-file.zip`,
  `${H1} *app-release.apk`,
  `${H1}  app-release.apk\r\n${H2}  other-file.zip\r\n`,
  `${H1}\tapp-release.apk`,
  `${H1}　app-release.apk`,
  `${H1}\u0085app-release.apk`,
  `${H1}  *`,
  `${H1}  **x`,
  `${H1}`,
  `${H1}  `,
  `${H1}0  app-release.apk`,
  `${H1.slice(0, 63)}  app-release.apk`,
  `${'g'.repeat(64)}  app-release.apk`,
  `  ${H1}  app-release.apk  `,
  `﻿${H1}  app-release.apk`,
  `${H1}  app-release.apk `,
  `${H1}  app release.apk`,
  `${H1}  my app.apk\n${H2}  *star name.apk`,
  `${H1}  dup.apk\n${H2}  dup.apk`,
  `${H1}  a.apk\rb\n${H2}  b.apk`,
  `# comment\n\n${H1}  app-release.apk`,
  '',
  `${H1}  日本語.apk`,
];
const names = ['app-release.apk', 'other-file.zip', '*', '*x', 'x', 'my app.apk', 'star name.apk', 'dup.apk', 'a.apk', 'b.apk', '日本語.apk', 'missing.apk', '', 'app release.apk', 'app-release.apk '];
const checksumCases = [];
for (const text of texts) for (const name of names) checksumCases.push({ text, name, result: parseChecksumsFile(text, name) });

mkdirSync(join(here, '../tests/data'), { recursive: true });
writeFileSync(join(here, '../tests/data/appupdate_golden.json'), JSON.stringify({ updateCases, checksumCases }));
console.log(`${updateCases.length} update cases (${updateCases.filter((c) => c.result === true).length} newer), ${checksumCases.length} checksum cases (${checksumCases.filter((c) => c.result).length} found)`);
