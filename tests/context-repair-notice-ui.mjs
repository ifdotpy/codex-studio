#!/usr/bin/env node
// Render the actual banner: ordinary monitor waits are quiet, failures stay visible.
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
const cache = await mkdtemp(join(tmpdir(), "context-repair-notice-vite-"));
const entry = join(repo, "web/__context-repair-notice-fixture.jsx");
const source = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import {NativeError} from '/src/components/NativeNotice.tsx';
function Fixture(){
  const [snapshot,setSnapshot]=useState(null);
  window.applySnapshot=setSnapshot;
  return <MantineProvider><main data-case={snapshot?.label}>
    {snapshot && <NativeError agent={snapshot.agent}/>}
    <output>{JSON.stringify(snapshot?.agent)}</output>
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
      name: "context-repair-notice-fixture",
      resolveId(id) {
        if (id === "/__context-repair-notice-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__context-repair-notice-fixture.jsx"></script>',
          );
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
  const page = await browser.newPage();
  const errors = [],
    writes = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/**", async (route) => {
    if (route.request().method() !== "GET") writes.push(route.request().url());
    await route.fulfill({ json: {} });
  });
  await page.goto(server.resolvedUrls.local[0]);
  await page.waitForFunction(() => typeof window.applySnapshot === "function");
  const error =
    "Context repair waits for monitors: 3c403ca5-4143-5750-a061-dd19ab5aab5c";
  const agent = {
    id: "fixture",
    name: "Fixture",
    source: "native",
    status: "queued",
    model: "gpt-6-astra",
    created: 1,
    threadId: "native-fixture",
    accountKey: "default",
    epoch: 0,
    error,
    contextRepairWait: { scope: "local", error },
  };
  let cases = 0;
  const check = async (label, value, visible, text) => {
    await page.evaluate((snapshot) => window.applySnapshot(snapshot), {
      label,
      agent: value,
    });
    await page.locator(`main[data-case="${label}"]`).waitFor();
    assert.equal(
      await page.locator(".native-error").count(),
      visible ? 1 : 0,
      label,
    );
    if (text)
      assert.match(await page.locator(".native-error").innerText(), text);
    assert.deepEqual(
      JSON.parse(await page.locator("output").innerText()),
      JSON.parse(JSON.stringify(value)),
      "Presentation preserves saved state",
    );
    cases++;
  };
  await check("ordinary-monitor", agent, false);
  await check(
    "missing-proof",
    { ...agent, contextRepairWait: undefined },
    true,
  );
  await check(
    "native-scope",
    { ...agent, contextRepairWait: { scope: "native", error } },
    true,
  );
  await check(
    "unknown-scope",
    { ...agent, contextRepairWait: { error } },
    true,
  );
  await check(
    "mismatched-error",
    { ...agent, error: "disk I/O error" },
    true,
    /disk I\/O error/,
  );
  await check("failed-state", { ...agent, status: "failed" }, true);
  const receiptError =
    "Context repair waits for the existing native recovery receipt";
  await check(
    "unknown-receipt",
    {
      ...agent,
      error: receiptError,
      contextRepairWait: { scope: "local", error: receiptError },
    },
    true,
    /receipt/,
  );
  await check(
    "native-block",
    {
      ...agent,
      nativeThreadBlock: {
        threadId: agent.threadId,
        error: {
          codexErrorInfo: "badRequest",
          message: "Native request rejected",
        },
      },
    },
    true,
    /Native request rejected/,
  );
  await check(
    "capacity-retry",
    {
      ...agent,
      capacityRetry: {
        id: "retry",
        threadId: agent.threadId,
        epoch: 0,
        accountKey: "default",
        status: "scheduled",
        dueAt: Date.now() / 1000 + 60,
      },
    },
    true,
    /Automatic retry/,
  );
  await check(
    "real-failure",
    {
      ...agent,
      error: {
        codexErrorInfo: "responseStreamDisconnected",
        message: "stream disconnected before completion",
      },
    },
    true,
  );
  await check("wait-after-failure", agent, false);
  assert.deepEqual(errors, []);
  assert.deepEqual(writes, []);
  console.log(
    JSON.stringify({ pass: true, cases, noWrites: true, realComponent: true }),
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
