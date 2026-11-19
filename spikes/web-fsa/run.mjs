// Drives the wasm spike in headless Chromium. Usage (after building, see README.md):
//   PLAYWRIGHT_DIR=D:/Android/playwright node run.mjs
// A real folder picker can't be automated, so the directory handle is an OPFS one: it is the same
// FileSystemDirectoryHandle interface the Rust code sees for a picked folder.
import { createRequire } from 'node:module';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(process.env.PLAYWRIGHT_DIR ?? here, 'noop.js'));
const { chromium } = require('playwright');

const types = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer(async (req, res) => {
  const path = req.url === '/' ? '/index.html' : req.url.split('?')[0];
  const file = path === '/index.html' ? join(here, 'index.html') : join(here, 'pkg', path);
  try {
    res.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream' });
    res.end(await readFile(file));
  } catch {
    res.writeHead(404).end();
  }
}).listen(0);
const url = `http://localhost:${server.address().port}/`;

const browser = await chromium.launch();
const page = await browser.newPage();
page.on('console', (m) => console.log('[page]', m.text()));
page.on('pageerror', (e) => console.log('[pageerror]', e.message));

await page.goto(url);
await page.waitForFunction(() => window.ready === true);
console.log('--- first load');
console.log(await page.evaluate(() => window.runSpike()));
console.log('showDirectoryPicker present:', await page.evaluate(() => typeof window.showDirectoryPicker));

// Persist a handle in IndexedDB, reload, and use the restored handle from Rust.
console.log('--- handle persistence');
console.log('seeded:', await page.evaluate(() => window.seedAndStore(25)));
await page.reload();
await page.waitForFunction(() => window.ready === true);
console.log(await page.evaluate(() => window.restoreAndCount()));

await browser.close();
server.close();
