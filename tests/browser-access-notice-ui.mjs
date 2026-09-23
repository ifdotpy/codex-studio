#!/usr/bin/env node
// Render the production browser notice against account-specific desktop responses.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "browser-access-notice-vite-"));
const screenshot = join(tmpdir(), "codex-browser-access-notice.png");
const entry = join(repo, "web/__browser-access-notice-fixture.jsx");
const source = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import BrowserAccessNotice from '/src/components/BrowserAccessNotice.tsx';
function Fixture(){
  const [snapshot,setSnapshot]=useState({accountKey:'default',active:true});
  window.applySnapshot=setSnapshot;
  return <MantineProvider><main data-account={snapshot.accountKey}>
    <h2>Conversation</h2><BrowserAccessNotice {...snapshot}/>
  </main></MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Fixture/>);`;
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: join(repo, "web"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "browser-access-notice-fixture",
      resolveId(id) {
        if (id === "/__browser-access-notice-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end('<div id="root"></div><script type="module" src="/__browser-access-notice-fixture.jsx"></script>');
        });
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
  const page = await browser.newPage({ viewport: { width: 900, height: 520 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const requests = [];
  await page.route("**/api/desktop?**", async (route) => {
    const url = new URL(route.request().url());
    const key = url.searchParams.get("account_key");
    requests.push(key);
    const browserStatus =
      key === "disabled"
        ? { enabled: false, reason: "The Chrome plugin is disabled in this account's Codex configuration" }
        : { enabled: true, reason: null };
    await route.fulfill({ json: { browser: browserStatus } });
  });
  await page.goto(server.resolvedUrls.local[0]);
  await page.waitForFunction(() => typeof window.applySnapshot === "function");
  const notice = page.getByRole("status");
  const set = async (snapshot) => {
    await page.evaluate((value) => window.applySnapshot(value), snapshot);
  };
  await set({ accountKey: "disabled", active: true });
  await notice.waitFor();
  await assert.doesNotReject(() =>
    notice.getByText(/Browser access is off: The Chrome plugin is disabled/).waitFor(),
  );
  await page.screenshot({ path: screenshot });
  await set({ accountKey: "default", active: true });
  await page.waitForFunction(() => document.querySelector("[role=status]") === null);
  await set({ accountKey: "disabled", active: false });
  await page.waitForFunction(() => document.querySelector("[role=status]") === null);
  assert.deepEqual(requests, ["default", "disabled", "default"]);
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ pass: true, cases: 3, screenshot, requests }));
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
