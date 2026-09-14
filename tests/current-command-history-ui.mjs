#!/usr/bin/env node
// Completed command rows disappear. Current commands and source records remain.
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
const cache = await mkdtemp(join(tmpdir(), "current-command-history-vite-"));
const entry = join(repo, "web/__current-command-history-fixture.jsx");
const source = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import TurnHistory from '/src/components/TurnHistory.tsx';
function Fixture(){
 const [snapshot,setSnapshot]=useState(null);window.applySnapshot=setSnapshot;
 return <MantineProvider><main data-case={snapshot?.label}>
 {snapshot && <TurnHistory items={snapshot.items} enabled={snapshot.enabled} storageKey="fixture"
 renderMessage={item=><p data-message={item.id}>{item.text}</p>} onJump={()=>{}}/>}
 <output hidden>{JSON.stringify(snapshot?.items)}</output>
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
      name: "current-command-history-fixture",
      resolveId(id) {
        if (id === "/__current-command-history-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__current-command-history-fixture.jsx"></script>',
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
  const command = (id, state, type = "commandExecution", tool) => ({
    id,
    role: "tool",
    turnId: "turn",
    toolStatus: state,
    text: JSON.stringify({ type, tool, command: "echo " + id, status: state }),
  });
  const completed = command("finished", "completed");
  const running = command("current", "running");
  const items = [
    { id: "user", role: "user", text: "Run checks" },
    completed,
    command("failed", "failed"),
    command("monitor", "completed", "dynamicToolCall", "orchestration_monitor"),
    running,
    {
      id: "image",
      role: "tool",
      turnId: "turn",
      toolStatus: "completed",
      text: JSON.stringify({ type: "imageView", path: "/tmp/image.png" }),
    },
    {
      id: "answer",
      role: "assistant",
      turnId: "turn",
      phase: "final_answer",
      text: "The result is ready.",
    },
  ];
  let cases = 0;
  const apply = async (label, rows, enabled) => {
    await page.evaluate((snapshot) => window.applySnapshot(snapshot), {
      label,
      items: rows,
      enabled,
    });
    await page.locator(`main[data-case="${label}"]`).waitFor({state:"attached"});
    for (const disclosure of await page
      .locator(".turn-work,.tool-group")
      .all()) {
      if ((await disclosure.getAttribute("open")) === null)
        await disclosure.locator(":scope > summary").click();
    }
    assert.deepEqual(
      JSON.parse(await page.locator("output").textContent()),
      rows,
      "Source history remains intact",
    );
    cases++;
  };
  for (const enabled of [true, false]) {
    await apply("running-" + enabled, items, enabled);
    assert.equal(await page.locator('[data-message="current"]').count(), 1);
    for (const id of ["finished", "failed", "monitor"])
      assert.equal(await page.locator(`[data-message="${id}"]`).count(), 0);
    assert.equal(await page.locator('[data-message="answer"]').count(), 1);
    assert.equal(await page.locator('[data-message="image"]').count(), 1);
    assert.doesNotMatch(
      await page.locator("main").innerText(),
      /Ran 3 commands/,
    );
    const ended = items.map((x) =>
      x.id === "current" ? command("current", "completed") : x,
    );
    await apply("ended-" + enabled, ended, enabled);
    assert.equal(await page.locator('[data-message="current"]').count(), 0);
    await apply("only-history-" + enabled, [completed], enabled);
    assert.equal(
      await page.locator(".turn-work,.tool-group,.turn-history").count(),
      0,
    );
    await apply(
      "approval-" + enabled,
      [command("approval", "approval")],
      enabled,
    );
    assert.equal(await page.locator('[data-message="approval"]').count(), 1);
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await apply("mobile", items, true);
  assert.equal(await page.locator('[data-message="current"]').count(), 1);
  assert.equal(await page.locator('[data-message="finished"]').count(), 0);
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
