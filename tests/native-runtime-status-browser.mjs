import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-native-runtime-ui-"));
const entry = join(root, "native-runtime-fixture.tsx");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "native-runtime-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<html><body><div id="root"></div><script type="module" src="/native-runtime-fixture.tsx"></script></body></html>',
          );
        });
      },
      resolveId(id) {
        if (id === "/native-runtime-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';
import NativeRuntimeStatus from '/src/components/NativeRuntimeStatus.tsx';import Accounts from '/src/components/Accounts.tsx';import '/src/studio-theme.css';import '/src/appearance.css';
const accounts=[{id:'default',label:'Personal',status:'ready'},{id:'work',label:'Work with a very long account label for narrow screens',status:'ready'}];
function Fixture(){const [opened,setOpened]=useState(false);const [integrated,setIntegrated]=useState(false);window.setRuntimeOpen=setOpened;window.showAccounts=()=>setIntegrated(true);
return <MantineProvider><main style={{maxWidth:600,padding:12}}>{integrated?<Accounts state={{data:{accounts,defaultAccountKey:'default'},setData:()=>{},refresh:async()=>{},error:'',scope:'fixture'}} accountKey='default' changeAccount={async()=>{}} onError={()=>{}}/>:<NativeRuntimeStatus opened={opened} accounts={accounts}/>}</main></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
      },
    },
  ],
});
const initial = {
  status: "ready",
  checkedAt: 1790092579,
  selected: {
    version: "0.116.0",
    sourcePath: "/private/very/long/path/codex",
    sha256: "fixture",
  },
  candidates: [
    {
      version: "0.117.0",
      path: "/private/rejected/codex",
      status: "rejected",
      error: "Required method thread/resume is missing",
    },
    { version: "0.116.0", path: "/private/approved/codex", status: "approved" },
  ],
  accounts: {
    default: { status: "current", version: "0.116.0" },
    work: {
      status: "waiting",
      version: "0.115.0",
      targetVersion: "0.116.0",
      reason: "Waiting for active turns to finish",
    },
  },
};
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1100, height: 850 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let reads = 0;
  let body = { nativeRuntime: initial };
  await page.route("**/api/**", (route) => {
    assert.equal(route.request().method(), "GET", "Status must stay read-only");
    if (new URL(route.request().url()).pathname === "/api/desktop") {
      reads++;
      return route.fulfill({ json: body });
    }
    return route.fulfill({ json: { accounts: [], data: null } });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.waitForFunction(() => !!window.setRuntimeOpen);
  await page.clock.install();
  await page.clock.runFor(12000);
  assert.equal(reads, 0, "Closed panel must not read diagnostics");
  await page.evaluate(() => window.setRuntimeOpen(true));
  const panel = page.getByRole("region", { name: "Codex runtime" });
  await panel.waitFor();
  assert.match(await panel.innerText(), /0\.116\.0/);
  assert.match(await panel.innerText(), /1 account pending/);
  assert.match(await panel.innerText(), /Personal/);
  assert.match(await panel.innerText(), /Waiting for active turns to finish/);
  assert.match(
    await panel.innerText(),
    /Version 0\.117\.0 rejected: Required method thread\/resume is missing/,
  );
  assert.equal(
    await panel.locator("time").getAttribute("datetime"),
    new Date(initial.checkedAt * 1000).toISOString(),
  );
  assert.equal(
    (await panel.innerText()).includes("/private/"),
    false,
    "Do not show verbose source paths",
  );
  assert.equal(await panel.getByRole("button").count(), 0);

  body = {
    nativeRuntime: {
      ...initial,
      accounts: { work: { status: "updating", targetVersion: "0.116.0" } },
    },
  };
  await page.clock.runFor(10100);
  await page.waitForFunction(() =>
    document
      .querySelector(".native-runtime-accounts")
      ?.textContent.includes("Updating"),
  );
  assert.equal(reads, 2);
  body = {
    nativeRuntime: {
      ...initial,
      status: "failed",
      accounts: { work: { status: "failed", reason: "Handshake failed" } },
    },
  };
  await page.clock.runFor(10100);
  await page.waitForFunction(() =>
    document
      .querySelector(".native-runtime-status")
      ?.textContent.includes("Handshake failed"),
  );
  assert.match(await panel.innerText(), /Runtime check failed/);
  assert.match(await panel.innerText(), /Update failed/);
  body = {
    nativeRuntime: {
      ...initial,
      status: "checking",
      selected: null,
      checkedAt: 0,
      candidates: [],
      accounts: {},
    },
  };
  await page.clock.runFor(10100);
  await page.waitForFunction(() =>
    document
      .querySelector(".native-runtime-status")
      ?.textContent.includes("Checking installed versions"),
  );
  assert.equal(await panel.locator("time").count(), 0);

  body = {};
  await page.clock.runFor(10100);
  await panel.waitFor({ state: "detached" });
  await page.evaluate(() => window.setRuntimeOpen(false));
  const beforeClose = reads;
  await page.clock.runFor(30000);
  assert.equal(reads, beforeClose, "Close must stop polling");

  // Ignore abort in this request to prove that a late response cannot replace a new open's data.
  await page.evaluate(() => {
    const original = window.fetch;
    window.fetch = (...args) => {
      if (args[0] === "/api/desktop") {
        window.fetch = original;
        window.delayedRuntimeSignal = args[1].signal;
        return new Promise((resolve) => {
          window.finishOldRuntimeRead = resolve;
        });
      }
      return original(...args);
    };
    window.setRuntimeOpen(true);
  });
  await page.waitForFunction(() => !!window.finishOldRuntimeRead);
  await page.evaluate(() => window.setRuntimeOpen(false));
  await page.waitForFunction(() => window.delayedRuntimeSignal.aborted);
  body = { nativeRuntime: initial };
  await page.evaluate(() => window.setRuntimeOpen(true));
  await panel.waitFor();
  await page.evaluate(() =>
    window.finishOldRuntimeRead(
      new Response(JSON.stringify({ nativeRuntime: null }), {
        headers: { "Content-Type": "application/json" },
      }),
    ),
  );
  await page.clock.runFor(100);
  assert.equal(await panel.count(), 1, "Old response must not erase new state");
  await page.setViewportSize({ width: 320, height: 720 });
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
    true,
    "Panel must fit mobile width",
  );
  assert.equal(
    await panel.evaluate((el) => el.scrollWidth <= el.clientWidth),
    true,
  );

  await page.evaluate(() => window.showAccounts());
  await page.getByTestId("account-picker").click();
  await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
  await page
    .getByRole("dialog", { name: "Accounts", exact: true })
    .getByRole("region", { name: "Codex runtime" })
    .waitFor();
  assert.equal(
    await panel.evaluate((el) => el.scrollWidth <= el.clientWidth),
    true,
    "Panel must also fit inside the mobile Accounts dialog",
  );
  if (process.env.SCREENSHOT)
    await panel.screenshot({ path: process.env.SCREENSHOT });
  assert.deepEqual(errors, []);
  console.log(
    "PASS native runtime status: lifecycle, cancellation, stale responses, refresh, failures, labels, mobile width, Accounts integration",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
