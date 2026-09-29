// Captures README screenshots of the search window without running Tauri or
// Windows. The front end (src/) runs as-is in headless Chromium; mock.js
// stands in for the Rust backend with a made-up two-drive result set. What
// you see is the real UI; only the search results are fabricated.
//
//   pnpm screenshots
//
// Writes docs/screenshots/<scene>.png. First run needs a browser:
//   pnpm exec playwright install chromium
import { chromium } from 'playwright';
import http from 'node:http';
import { mkdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, extname, join, resolve, sep } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const SRC = join(ROOT, 'src');
const OUT = join(ROOT, 'docs', 'screenshots');
const MOCK = await readFile(join(HERE, 'mock.js'), 'utf8');

// Matches the window's default size (src-tauri/tauri.conf.json), captured at 2x.
const W = 900;
const H = 560;

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' };

const server = http.createServer(async (req, res) => {
  const { pathname } = new URL(req.url, 'http://localhost');
  const path = pathname === '/' ? '/index.html' : pathname;
  try {
    // Serve only files under src/; a request like /../package.json must 404.
    const file = resolve(SRC, '.' + decodeURIComponent(path));
    if (!file.startsWith(SRC + sep)) throw new Error('outside src/');
    const body = await readFile(file);
    res.writeHead(200, { 'Content-Type': MIME[extname(path)] || 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((done) => server.listen(0, '127.0.0.1', done));
const url = `http://127.0.0.1:${server.address().port}/index.html`;

await mkdir(OUT, { recursive: true });
const browser = await chromium.launch();

async function shoot(name, { mode, query }) {
  const ctx = await browser.newContext({ viewport: { width: W, height: H }, deviceScaleFactor: 2 });
  const page = await ctx.newPage();
  await page.addInitScript(MOCK);
  await page.goto(url);
  await page.waitForSelector('#queryInput');

  const input = page.locator('#queryInput');
  // Clear and switch mode first — with an empty box this dispatches no
  // search (pattern.length < MIN_QUERY_CHARS), so only the typed query below
  // ever fires one, at whatever debounce that mode uses (content is slower).
  await input.fill('');
  if (mode) {
    const order = ['files', 'content', 'both', 'folders']; // src/app.js's MODE_CYCLE
    for (let i = 0; i < order.indexOf(mode); i++) await page.keyboard.press('Tab');
  }
  await input.type(query, { delay: 20 });
  await page.waitForTimeout(mode === 'content' ? 800 : 300);
  await page.screenshot({ path: join(OUT, `${name}.png`) });
  await ctx.close();
  console.log(name);
}

try {
  await shoot('filenames', { mode: 'files', query: 'index' });
  await shoot('content', { mode: 'content', query: 'index' });
} finally {
  await browser.close();
  server.close();
}
