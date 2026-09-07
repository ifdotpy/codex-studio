// Replicated state uses small credential refreshes, including legacy fallback.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
const require = createRequire(new URL('../web/package.json', import.meta.url));
const { chromium } = require('playwright-core');
const { createServer } = await import(new URL('../web/node_modules/vite/dist/node/index.js', import.meta.url));
const server = await createServer({ configFile: false, root: fileURLToPath(new URL('../web', import.meta.url)), server: { host: '127.0.0.1', port: 0 } });
await server.listen();
const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
try {
 const page = await browser.newPage(), errors = [];
 page.on('pageerror', error => errors.push(error.message));
 let snapshots = 0, sessions = 0, status = 200, token = 'first';
 const workspaceId = 'a'.repeat(32);
 await page.route('**/check', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>Session refresh</title>' }));
 await page.route('**/api/state', route => { snapshots++; return route.fulfill({ json: { token, marker: 'http' } }); });
 await page.route('**/api/session', route => { sessions++; return route.fulfill({ status, json: status === 200 ? { token } : { error: 'Session unavailable' } }); });
 await page.route('**/api/sync/identity', route => route.fulfill({ json: { workspaceId } }));
 await page.route('**/api/sync/stream', route => route.fulfill({ contentType: 'text/event-stream', body: 'data: "RESYNC"\n\n' }));
 await page.route('**/api/sync/pull?*', route => {
  const after = Number(new URL(route.request().url()).searchParams.get('after'));
  return route.fulfill({ json: { workspaceId, documents: after ? [] : [{ id: 'state', seq: 1, _deleted: false, payload: JSON.stringify({ marker: 'replicated' }) }], checkpoint: { seq: 1 } } });
 });
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
 await page.evaluate(async () => {
  const { useSnapshot } = await import('/src/hooks.ts');
  const r = await import('/node_modules/.vite/deps/react.js'), d = await import('/node_modules/.vite/deps/react-dom_client.js');
  const React = r.default || r, { createRoot } = d.default || d;
  const node = document.createElement('div'); document.body.appendChild(node);
  createRoot(node).render(React.createElement(function Harness() { window.snapshot = useSnapshot(); return null; }));
 });
 await page.waitForFunction(() => window.snapshot?.data?.marker === 'replicated');
 const deadline = Date.now() + 10000;
 while (sessions < 2 && Date.now() < deadline) await page.waitForTimeout(100);
 assert.ok(sessions >= 2);
 assert.equal(snapshots, 1, 'Periodic replication must not download the full state again');
 token = 'rotated';
 await page.waitForFunction(() => window.snapshot.data.token === 'rotated');
 assert.equal(await page.evaluate(() => window.snapshot.data.marker), 'replicated');
 status = 503;
 await page.waitForFunction(() => window.snapshot.error === 'Session unavailable');
 assert.equal(snapshots, 1, 'A server failure is visible, not hidden by fallback');
 status = 404; token = 'legacy-fallback';
 await page.waitForFunction(() => window.snapshot.data.token === 'legacy-fallback');
 assert.ok(snapshots > 1);
 assert.equal(await page.evaluate(() => window.snapshot.data.marker), 'replicated');
 assert.equal(await page.evaluate(() => window.snapshot.error), '');
 assert.deepEqual(errors, []);
 console.log('session polling PASS: no repeated full snapshot, rotated token, visible error, legacy fallback');
} finally { await browser.close(); await server.close(); }
