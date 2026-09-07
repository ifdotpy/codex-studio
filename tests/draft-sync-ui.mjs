// Two browser profiles, real SQLite/RxDB replication, no live model calls.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, 'web/package.json'))('playwright-core');
const state = await mkdtemp(join(tmpdir(), 'studio-draft-sync-'));
const fixture = spawn('python3', ['-B', join(root, 'tests/simple-ui-fixture.py'), state], { stdio: ['pipe', 'pipe', 'pipe'] });
let browser, log = '';
fixture.stderr.on('data', chunk => { log += chunk; });
try {
 const port = await new Promise((resolve, reject) => {
  fixture.stdout.once('data', chunk => resolve(Number(String(chunk).trim())));
  fixture.once('exit', () => reject(new Error(log)));
 });
 const origin = `http://127.0.0.1:${port}`;
 const snapshot = await (await fetch(origin + '/api/state')).json();
 const { workspaceId } = await (await fetch(origin + '/api/sync/identity')).json();
 const session = snapshot.runtime.agents.find(agent => agent.name === 'Other project').id;
 const documents = async () => (await (await fetch(origin + '/api/sync/pull?scope=drafts&after=0&limit=100')).json()).documents;
 const push = async (value, previous) => {
  const response = await fetch(origin + '/api/sync/drafts', {
   method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Canvas-Token': snapshot.token, 'X-Canvas-Workspace': workspaceId, Origin: origin },
   body: JSON.stringify({ rows: [{ newDocumentState: { id: value.id, seq: 0, payload: JSON.stringify(value) }, ...(previous ? { assumedMasterState: previous } : {}) }] }),
  });
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), []);
 };
 browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' });
 const desktop = await browser.newPage({ viewport: { width: 1280, height: 850 } });
 const phone = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
 const errors = [];
 for (const page of [desktop, phone]) page.on('pageerror', error => errors.push(error.message));
 await desktop.goto(origin);
 await desktop.locator('#message').fill('Draft from desktop');
 const waitText = (page, text) => page.waitForFunction(text => document.querySelector('#message')?.value === text, text, { timeout: 15000 });
 await phone.goto(origin);
 await waitText(phone, 'Draft from desktop');
 await phone.locator('#message').fill('Continue on phone');
 await waitText(desktop, 'Continue on phone');
 assert.equal(await desktop.getByRole('button', { name: /^Other drafts/ }).count(), 0);
 await desktop.reload();
 await waitText(desktop, 'Continue on phone');
 await phone.locator('#message').fill('');
 await waitText(desktop, '');
 await desktop.reload();
 await waitText(desktop, '');
 assert.equal(await desktop.getByRole('button', { name: /^Other drafts/ }).count(), 0, 'Cleared drafts must not return as conflicts');
 await phone.locator('#message').fill('Current draft');
 await waitText(desktop, 'Current draft');
 // Independent old branches model concurrent edits from offline profiles.
 const stamp = Date.now() - 10000;
 for (let i = 0; i < 12; i++) await push({ id: `offline-${i}:${session}`, session, device: `offline-${i}`, text: `Alternative ${i}\n` + 'Long draft text '.repeat(700), updated: stamp + i });
 await phone.getByRole('button', { name: 'Other drafts (12)', exact: true }).waitFor({ timeout: 15000 });
 assert.equal(await phone.locator('#message').inputValue(), 'Current draft');
 assert.equal(await phone.locator('.sync-notices details').count(), 0);
 const before = await phone.locator('#composer').boundingBox();
 await phone.getByRole('button', { name: 'Other drafts (12)', exact: true }).click();
 const panel = phone.locator('.draft-versions');
 await panel.waitFor();
 const bounds = await panel.boundingBox();
 assert.ok(bounds.height <= 321 && bounds.width <= 366, JSON.stringify(bounds));
 const after = await phone.locator('#composer').boundingBox();
 assert.ok(Math.abs(before.y - after.y) < 1, 'Conflict preview must not shrink the chat');
 await phone.waitForTimeout(250);
 await phone.screenshot({ path: join(state, 'draft-versions.png') });
 await phone.getByRole('button', { name: 'Dismiss', exact: true }).first().click();
 await phone.getByRole('button', { name: 'Dismiss all', exact: true }).click();
 assert.equal(await phone.getByRole('button', { name: /^Other drafts/ }).count(), 0);
 await phone.reload();
 await waitText(phone, 'Current draft');
 await phone.waitForTimeout(3500);
 assert.equal(await phone.getByRole('button', { name: /^Other drafts/ }).count(), 0, 'Dismissal must survive reload and replication');
 // New content on a dismissed branch remains available.
 const previous = (await documents()).find(doc => doc.id === `offline-0:${session}`);
 await push({ ...JSON.parse(previous.payload), text: 'Changed alternative', updated: stamp + 100 }, previous);
 await phone.getByRole('button', { name: 'Other drafts (1)', exact: true }).waitFor({ timeout: 15000 });
 await phone.getByRole('button', { name: 'Other drafts (1)', exact: true }).click();
 await phone.getByRole('button', { name: 'Use this draft', exact: true }).click();
 await waitText(phone, 'Changed alternative');
 await waitText(desktop, 'Changed alternative');
 assert.equal(await phone.getByRole('button', { name: /^Other drafts/ }).count(), 0);
 assert.equal(await desktop.getByRole('button', { name: /^Other drafts/ }).count(), 0);
 assert.deepEqual(errors, []);
 console.log(`PASS: two-device draft handoff, clear, reload, bounded mobile preview, durable dismissal, replacement. ${state}`);
} finally {
 if (browser) await browser.close();
 fixture.stdin.end();
 await new Promise(resolve => { fixture.once('exit', resolve); setTimeout(() => { fixture.kill('SIGTERM'); resolve(); }, 3000).unref(); });
}
