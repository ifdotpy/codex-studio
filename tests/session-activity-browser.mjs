import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const cacheDir = await mkdtemp(join(tmpdir(), "studio-session-activity-"));
const entry = join(root, "session-activity-fixture.tsx");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  optimizeDeps: {
    noDiscovery: true,
    include: [
      "react",
      "react/jsx-runtime",
      "react/jsx-dev-runtime",
      "react-dom/client",
      "react-dom",
    ],
  },
  plugins: [
    {
      name: "session-activity-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<style>:root{--surface:#1b1b20;--text:#e9e9ee;--muted:#9696a5;--line:#ffffff0d}body{margin:0;font:14px system-ui;color:var(--text);background:var(--surface)}header,main{padding:16px}*{box-sizing:border-box}</style><div id="root"></div><script type="module" src="/session-activity-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/session-activity-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `import React,{useState}from'react';import{createRoot}from'react-dom/client';import{flushSync}from'react-dom';import SessionActivity from'/src/components/SessionActivity.tsx';
const now=Date.now()/1000;window.opened=[];window.time=Date.now();Date.now=()=>window.time;
const items=[{id:'long-command',kind:'task',agentId:'paused-child',agentName:'webcrypto-globals',label:'Running command',command:'find /Users/igor '+ 'directory/'.repeat(35),created:now-14*3600,status:'running'},{id:'monitor-one',kind:'monitor',agentId:'lead',agentName:'Main chat',label:'Waiting for a monitor',command:'watch build',created:now-120,status:'running'},{id:'worker-one',kind:'agent',agentId:'worker-one',agentName:'Worker one',label:'Working',status:'running'},{id:'tool-two',kind:'task',agentId:'worker-two',agentName:'Worker with a very long name that must fit the mobile viewport',label:'Using tools',created:now-30,status:'starting'}];
function Fixture(){const[activities,setActivities]=useState(items);window.setActivities=value=>flushSync(()=>setActivities(value));window.activities=activities;return <><header>Main chat</header><SessionActivity activities={activities} onOpen={value=>window.opened.push(value)}/><main>Conversation</main></>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  const strip = page.getByRole("region", { name: "Session activity" });
  await strip.waitFor();
  assert.equal(await strip.locator("[data-activity-id]").count(), 1);
  await strip.getByText("webcrypto-globals", { exact: true }).waitFor();
  assert.equal(
    await strip.getByText("Running command", { exact: true }).count(),
    0,
  );
  await strip.getByText("14h 0m", { exact: true }).waitFor();
  assert.equal(await strip.locator("code").count(), 0);
  assert(
    await strip.evaluate((node) => node.getBoundingClientRect().height <= 34),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  assert(
    await strip.evaluate((node) => node.getBoundingClientRect().height <= 34),
  );
  await page.screenshot({
    path: join(cacheDir, "session-activity-compact.png"),
  });
  await strip.locator('[data-activity-id="long-command"]').click();
  assert.deepEqual(
    await page.evaluate(() => window.opened[0]),
    await page.evaluate(() => window.activities[0]),
  );
  await strip.getByRole("button", { name: "Show 3 more", exact: true }).click();
  assert.equal(await strip.locator("[data-activity-id]").count(), 4);
  for (const id of ["monitor-one", "worker-one", "tool-two"])
    await strip.locator(`[data-activity-id="${id}"]`).click();
  assert.deepEqual(
    await page.evaluate(() => window.opened.map((item) => item.id)),
    ["long-command", "monitor-one", "worker-one", "tool-two"],
  );
  await page.setViewportSize({ width: 390, height: 844 });
  assert(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
  assert(await strip.evaluate((node) => node.scrollWidth <= node.clientWidth));
  await page.screenshot({
    path: join(cacheDir, "session-activity-mobile.png"),
  });
  await page.evaluate(() =>
    window.setActivities(
      Array.from({ length: 40 }, (_, index) => ({
        ...window.activities[index % 4],
        id: `many-${index}`,
      })),
    ),
  );
  assert(
    await strip
      .getByRole("list")
      .evaluate(
        (node) =>
          node.clientHeight <= 240 &&
          node.clientHeight <= innerHeight * 0.3 &&
          node.scrollHeight > node.clientHeight,
      ),
  );
  await strip.getByRole("button", { name: "Show less", exact: true }).click();
  assert.equal(await strip.locator("[data-activity-id]").count(), 1);
  await page.evaluate(() => {
    window.time += 120000;
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await strip.getByText("14h 2m", { exact: true }).waitFor();
  await page.evaluate(() => window.setActivities([window.activities[0]]));
  assert.equal(await strip.getByRole("button", { name: /Show/ }).count(), 0);
  await page.evaluate(() => window.setActivities([]));
  await strip.waitFor({ state: "detached" });
  assert.deepEqual(errors, []);
  console.log(
    `PASS session activity: ${browserType.name()}, owners, command, elapsed, exact actions, one compact row, expand, 390px, empty state`,
  );
  console.log("Screenshot:", join(cacheDir, "session-activity-mobile.png"));
} finally {
  await browser?.close();
  await server.close();
}
