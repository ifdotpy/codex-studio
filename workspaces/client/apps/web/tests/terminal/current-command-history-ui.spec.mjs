// Completed command rows disappear. Current commands and source records remain.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { test } from "../playwright.mjs";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("current command history ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(repo, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const cache = await mkdtemp(join(tmpdir(), "current-command-history-vite-"));
  const entry = join(
    repo,
    "workspaces/client/apps/web/__current-command-history-fixture.jsx",
  );
  const source = `
  import React, {useState} from 'react';
  import {createRoot} from 'react-dom/client';
  import {MantineProvider} from '@mantine/core';
  import '@mantine/core/styles.css';
  import TurnHistory from '/src/components/conversation/transcript/TurnHistory.tsx';
  function Fixture(){
   const [snapshot,setSnapshot]=useState(null);window.applySnapshot=setSnapshot;
    return <MantineProvider><main data-case={snapshot?.label}>
   {snapshot && <TurnHistory key={snapshot.label} items={snapshot.items} currentTurn={snapshot.currentTurn} enabled={snapshot.enabled} storageKey="fixture"
   renderMessage={item=><p data-message={item.id}>{item.text}</p>} onJump={()=>{}}/>}
   <output hidden>{JSON.stringify(snapshot?.items)}</output>
   </main></MantineProvider>;
  }
  createRoot(document.getElementById('root')).render(<Fixture/>);`;
  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
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
    browser = _browser;
    const page = await browser.newPage();
    const errors = [],
      writes = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "GET")
        writes.push(route.request().url());
      await route.fulfill({ json: {} });
    });
    await page.goto(server.resolvedUrls.local[0]);
    await page.waitForFunction(
      () => typeof window.applySnapshot === "function",
    );
    const command = (id, state, type = "commandExecution", tool) => ({
      id,
      role: "tool",
      turnId: "turn",
      ...(["completed", "failed", "interrupted", "ended"].includes(state)
        ? { turnStatus: state }
        : {}),
      toolStatus: state,
      text: JSON.stringify({
        type,
        tool,
        command: "echo " + id,
        status: state,
      }),
    });
    const completed = {
      ...command("finished", "completed"),
      turnId: "previous",
    };
    const failed = {
      ...command("failed", "failed"),
      turnId: "previous",
    };
    const monitor = {
      ...command(
        "monitor",
        "completed",
        "dynamicToolCall",
        "orchestration_monitor",
      ),
      turnId: "previous",
      turnStatus: "failed",
    };
    const running = command("current", "running");
    const items = [
      { id: "user", role: "user", text: "Run checks" },
      completed,
      failed,
      monitor,
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
    const apply = async (label, rows, enabled, currentTurn) => {
      await page.evaluate((snapshot) => window.applySnapshot(snapshot), {
        label,
        items: rows,
        enabled,
        currentTurn,
      });
      await page
        .locator(`main[data-case="${label}"]`)
        .waitFor({ state: "attached" });
      assert.deepEqual(
        JSON.parse(await page.locator("output").textContent()),
        rows,
        "Source history remains intact",
      );
      cases++;
    };
    const expandWork = async () => {
      for (const disclosure of await page
        .locator(".turn-work,.tool-group")
        .all()) {
        if ((await disclosure.getAttribute("open")) === null)
          await disclosure.locator(":scope > summary").click();
      }
    };
    for (const enabled of [true, false]) {
      await apply("running-" + enabled, items, enabled, "turn");
      await expandWork();
      assert.equal(
        await page.locator('[data-message="current"]').isVisible(),
        true,
      );
      for (const id of ["finished", "failed", "monitor"])
        assert.equal(
          await page.locator(`[data-message="${id}"]`).isVisible(),
          false,
        );
      assert.equal(
        await page.locator('[data-message="answer"]').isVisible(),
        true,
      );
      assert.equal(
        await page.locator('[data-message="image"]').isVisible(),
        true,
      );
      assert.doesNotMatch(
        await page.locator("main").innerText(),
        /Ran 3 commands/,
      );
      const ended = items.map((x) =>
        x.turnId === "turn"
          ? {
              ...x,
              ...(x.id === "current" ? command("current", "completed") : {}),
              turnStatus: "completed",
            }
          : x,
      );
      await apply("ended-" + enabled, ended, enabled);
      await expandWork();
      assert.equal(
        await page.locator('[data-message="current"]').isVisible(),
        false,
      );
      await apply("only-history-" + enabled, [completed], enabled);
      assert.equal(
        await page.locator(".turn-work,.tool-group,.turn-history").count(),
        0,
      );
      await apply(
        "approval-" + enabled,
        [command("approval", "approval")],
        enabled,
        "turn",
      );
      assert.equal(await page.locator('[data-message="approval"]').count(), 1);
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await apply("mobile", items, true, "turn");
    await expandWork();
    assert.equal(
      await page.locator('[data-message="current"]').isVisible(),
      true,
    );
    assert.equal(
      await page.locator('[data-message="finished"]').isVisible(),
      false,
    );
    assert.deepEqual(errors, []);
    assert.deepEqual(writes, []);
    console.log(
      JSON.stringify({
        pass: true,
        cases,
        noWrites: true,
        realComponent: true,
      }),
    );
  } finally {
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
