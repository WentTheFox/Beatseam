// PLAYWRIGHT_DIR=<dir with playwright installed> node run.mjs   (after building, see README.md)
import { createRequire } from 'node:module';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const { chromium } = createRequire(join(process.env.PLAYWRIGHT_DIR ?? here, 'noop.js'))('playwright');
const files = {
  '/': ['index.html', 'text/html'],
  '/processor.js': ['processor.js', 'text/javascript'],
  '/mixer.wasm': ['target/wasm32-unknown-unknown/release/beatseam_spike_web_audio.wasm', 'application/wasm'],
};
const server = createServer(async (req, res) => {
  const f = files[req.url.split('?')[0]];
  if (!f) return void res.writeHead(404).end();
  res.writeHead(200, { 'content-type': f[1] });
  res.end(await readFile(join(here, f[0])));
}).listen(0);
const url = `http://localhost:${server.address().port}/`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const headed = process.env.HEADED === '1';
const browser = await chromium.launch({ headless: !headed, args: ['--autoplay-policy=no-user-gesture-required', ...(headed ? ['--mute-audio'] : [])] });
const context = await browser.newContext();
const page = await context.newPage();
page.on('pageerror', (e) => console.log('[pageerror]', e.message));
await page.goto(url);

console.log('--- offline render through the worklet vs a JS reference');
console.log(await page.evaluate(() => window.offline()));
console.log('--- cost of one 128-frame quantum (main thread, wasm)');
console.log(await page.evaluate(() => window.bench()));

console.log('--- realtime context');
console.log(await page.evaluate(() => window.start()));
await sleep(2000);
const before = await page.evaluate(() => window.probe());
console.log('foreground:', before);

// Put another tab in front so the first one is hidden, then measure how far the worklet got.
const other = await context.newPage();
await other.goto('about:blank');
await other.bringToFront();
await sleep(8000);
const during = await page.evaluate(() => window.probe());
console.log('while hidden:', during);
console.log(
  `worklet advanced ${(during.frames - before.frames) / 48000}s of audio (ctx clock ${(during.ctxTime - before.ctxTime).toFixed(2)}s) ` +
  `in ~8s hidden; main-thread timer ticks in that time: ${during.timerTicks - before.timerTicks} (100 ms interval, ~80 unthrottled)`,
);

await browser.close();
server.close();
