import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, 'web/package.json'))('playwright-core');
const directory = await mkdtemp(join(tmpdir(), 'studio-reasoning-history-'));
const child = spawn(process.env.CODEX_AGENTS_PYTHON || 'python3', ['-B', join(repo, 'tests/simple-ui-fixture.py'), directory]);
let browser, log = '';
child.stderr.on('data', data => log += data);
try {
  const port = await new Promise((resolve, reject) => {
    child.stdout.once('data', data => resolve(Number(String(data).trim())));
    child.once('exit', () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + '/api/state')).json();
  const lead = state.runtime.agents.find(a => a.name === 'Release lead');
  const base = await (await fetch(origin + '/api/transcript?id=' + lead.id)).json();
  const now = Date.now() / 1000;
  const items = [
    {id:'question', role:'user', text:'Check how to submit the notification.', turnId:'t'},
    {id:'search', role:'output', title:'webSearch', text:JSON.stringify({type:'webSearch', query:'Official submission instructions', status:'completed'}), turnId:'t', toolStatus:'completed'},
    {id:'thought', role:'reasoning', text:'', turnId:'t', reasoningMs:480000, reasoningSince:null, reasoningObservedAt:now},
    {id:'answer', role:'assistant', text:'I found the submission instructions. I am checking the required documents.', turnId:'t'},
    {id:'live-thought', role:'reasoning', text:'', turnId:'t', reasoningMs:0, reasoningSince:now - 120, reasoningObservedAt:now},
  ];
  const transcript = {...base, items, order:items.map(i => i.id), replace:true,
    agent:{...base.agent, turnId:'t', status:'running', inFlight:true, activity:{phase:'thinking'}}};
  browser = await chromium.launch({headless:true, executablePath:process.env.CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'});
  const page = await browser.newPage({viewport:{width:1440,height:1000}});
  const errors = []; page.on('pageerror', e => errors.push(e.message));
  await page.route('**/api/sync/identity', r => r.fulfill({status:404,json:{error:'Unsupported sync'}}));
  await page.route('**/api/transcript**', route => {
    if(new URL(route.request().url()).searchParams.get('id') !== lead.id) return route.continue();
    return route.request().url().includes('/stream?') ? route.fulfill({contentType:'text/event-stream',body:'data: '+JSON.stringify(transcript)+'\n\n'}) : route.fulfill({json:transcript});
  });
  await page.goto(origin);
  await page.locator('[data-chat]').filter({hasText:'Release lead'}).click();
  await page.getByText('Thought 8m 0s', {exact:true}).waitFor();
  assert.equal(await page.locator('.current-prompt').count(),0,'Old prompt is not pinned above the chat');
  const live = page.locator('[data-message="live-thought"]');
  await live.waitFor();
  const bounds = await live.boundingBox();
  await live.evaluate(el => el.dataset.sameNode = 'yes');
  assert.equal(await page.locator('.agent-phase').getByText('Thinking', {exact:true}).count(),0,'No duplicate Thinking status');
  await page.waitForFunction(() => /Thinking 2m [1-9]s/.test(document.querySelector('[data-message="live-thought"]').textContent));
  assert.equal((await live.boundingBox()).height,bounds.height);
  items.at(-1).reasoningSince = null;
  items.at(-1).reasoningMs = 123000;
  await page.getByText('Thought 2m 3s', {exact:true}).waitFor({timeout:15000});
  assert.equal(await live.getAttribute('data-same-node'),'yes','Status update preserves the DOM node');
  assert.equal(await page.locator('.reasoning-duration').count(),2);
  await page.screenshot({path:join(directory,'reasoning-desktop.png')});
  await page.setViewportSize({width:390,height:844});
  await page.screenshot({path:join(directory,'reasoning-mobile.png')});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false);
  assert.deepEqual(errors,[]);
  console.log('PASS reasoning history, timer, stable node, no duplicate status, mobile width. '+directory);
} finally { await browser?.close(); child.kill('SIGTERM'); }
