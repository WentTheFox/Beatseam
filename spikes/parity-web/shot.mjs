import { chromium } from 'playwright';
import { writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const [, , htmlPath, pngPath, ariaPath] = process.argv;
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2 });
await page.goto(pathToFileURL(htmlPath).href);
await page.evaluate(() => document.fonts.ready);
await page.screenshot({ path: pngPath });
if (ariaPath) writeFileSync(ariaPath, await page.locator('body').ariaSnapshot());
await browser.close();
console.log('wrote', pngPath);
