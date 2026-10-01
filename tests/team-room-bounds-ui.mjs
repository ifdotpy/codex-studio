import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-room-bounds-"));
const entry = join(root, "team-room-bounds-fixture.tsx");
const room = {
  id: "private:lead:worker",
  kind: "private",
  members: ["lead", "worker"],
  name: "Worker",
};
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "room-bounds-fixture",
      configureServer(vite) {
        vite.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<html><body><div id="root"></div><script type="module" src="/team-room-bounds-fixture.tsx"></script></body></html>',
          );
        });
      },
      resolveId(id) {
        if (id === "/team-room-bounds-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `
import React from 'react'; import {createRoot} from 'react-dom/client'; import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css'; import TeamChats from '/src/components/TeamChats.tsx'; import '/src/style.css'; import '/src/workspace-layout.css'; import '/src/appearance.css'; import {theme} from '/src/theme.ts';
const lead={id:'lead',name:'Lead',rootId:'lead',isLead:true,source:'managed',created:1,updated:1};
const worker={id:'worker',name:'Worker',rootId:'lead',isLead:false,source:'managed',created:1,updated:1};
const room={id:'private:lead:worker',kind:'private',members:['lead','worker'],name:'Worker',updated:1,lastMessage:{seq:1391,text:'room message 1391',created:1,sender:'worker'}};
const data={stateDir:'room-fixture',threads:[lead,worker],runtime:{rooms:[room],requests:[],complaints:[],projects:[],peerTeams:[]}};
const app=createRoot(document.getElementById('root'));
window.showLegacy=()=>app.render(<div className='team-chat-messages'>{Array.from({length:1391},(_,i)=><article className='team-message' key={i}><strong>Worker</strong><p>{'room message '+(i+1)+' '}{'x'.repeat(520)}</p></article>)}</div>);
window.showCurrent=()=>app.render(<MantineProvider theme={theme} defaultColorScheme='dark'><TeamChats data={data} leadId='lead' refresh={async()=>{}} notify={()=>{}}/></MantineProvider>);
window.showCurrent();`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({ viewport: { width: 900, height: 620 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const requests = [];
  await page.route("**/api/agent-chat?*", async (route) => {
    const url = new URL(route.request().url());
    const before = Number(url.searchParams.get("before"));
    const after = Number(url.searchParams.get("after"));
    requests.push({ before: before || null, after: after || null });
    let lo = 1292,
      hi = 1391;
    if (before) {
      hi = before - 1;
      lo = Math.max(1, hi - 99);
    } else if (after) {
      lo = after + 1;
      hi = Math.min(1391, lo + 99);
    }
    const messages = [];
    for (let seq = lo; seq <= hi; seq++)
      messages.push({
        id: `m${seq}`,
        seq,
        room: "private:lead:worker",
        sender: "worker",
        senderName: "Worker",
        text: `room message ${seq} ` + "x".repeat(120),
        created: seq,
        deliveries: {},
      });
    return route.fulfill({
      json: {
        room,
        messages,
        nextBefore: lo > 1 ? lo : null,
        nextAfter: hi < 1391 ? hi : null,
      },
    });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.evaluate(() => window.showLegacy());
  const legacyStart = Date.now();
  await page.waitForFunction(
    () => document.querySelectorAll(".team-message").length === 1391,
  );
  const legacyRenderMs = Date.now() - legacyStart;
  await page.evaluate(() => window.showCurrent());
  await page.locator('[data-room="private:lead:worker"]').click();
  const start = Date.now();
  await page
    .locator(".team-message")
    .filter({ hasText: "room message 1391" })
    .waitFor();
  const initialMs = Date.now() - start;
  const scroll = page.locator(".unified-message-scroll");
  await page.getByRole("button", { name: "Earlier messages" }).click();
  await page.getByRole("button", { name: "Earlier messages" }).click();
  await page.waitForFunction(
    () =>
      document.querySelector(".unified-message-scroll")?.dataset
        .roomRetained === "240",
  );
  const retained = await scroll.getAttribute("data-room-retained");
  const mounted = await page.locator(".team-message").count();
  assert.equal(retained, "240");
  assert.ok(mounted < 60, `mounted ${mounted} messages`);
  assert.ok(mounted > 0);
  assert.ok(await page.getByRole("button", { name: "Later messages" }).count());
  await page.getByRole("button", { name: "Later messages" }).click();
  await page.waitForFunction(
    () =>
      document.querySelector(".unified-message-scroll")?.dataset
        .roomRetained === "240",
  );
  assert.ok(requests.some((request) => request.before !== null));
  assert.ok(requests.some((request) => request.after !== null));
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      retainedMessages: Number(retained),
      mountedMessages: mounted,
      legacyMessages: 1391,
      legacyRenderMs,
      initialRenderMs: initialMs,
      requests,
    }),
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
