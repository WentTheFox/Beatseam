// PLAYWRIGHT_DIR=<dir with playwright and @sqlite.org/sqlite-wasm installed> node run.mjs
import { createRequire } from 'node:module';
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { join, dirname, extname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const pw = process.env.PLAYWRIGHT_DIR;
const { chromium } = createRequire(join(pw, 'noop.js'))('playwright');
const sqliteDist = join(pw, 'node_modules/@sqlite.org/sqlite-wasm/dist');
const types = { '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.html': 'text/html' };

const server = createServer(async (req, res) => {
  const p = req.url.split('?')[0];
  const file = p.startsWith('/sqlite/') ? join(sqliteDist, p.slice(8)) : join(here, p === '/' ? 'index.html' : p.slice(1));
  try {
    const body = await readFile(file);
    // Cross-origin isolation, needed only by the "opfs" VFS (SharedArrayBuffer); the app is served by our own
    // server, so it can send these.
    res.writeHead(200, {
      'content-type': types[extname(file)] ?? 'application/octet-stream',
      'cross-origin-opener-policy': 'same-origin',
      'cross-origin-embedder-policy': 'require-corp',
    });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
}).listen(0);

const browser = await chromium.launch();
const page = await browser.newPage();
page.on('pageerror', (e) => console.log('[pageerror]', e.message));
await page.goto(`http://localhost:${server.address().port}/`);
console.log('crossOriginIsolated:', await page.evaluate(() => crossOriginIsolated));
console.log('sqlite3.wasm bytes:', (await stat(join(sqliteDist, 'sqlite3.wasm'))).size);
for (const backend of ['sahpool', 'opfs', 'idb']) {
  const r = await page.evaluate((b) => new Promise((resolve) => {
    const w = new Worker('/worker.js', { type: 'module' });
    w.onmessage = (e) => { w.terminate(); resolve(e.data); };
    w.postMessage(b);
  }), backend);
  console.log(`--- ${backend}`);
  console.log(r.ok ? r.result : r.error);
}
await browser.close();
server.close();
