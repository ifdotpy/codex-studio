import type { HistorySnapshot } from "./current-command-history-fixture.tsx";
import type { Message } from "../../src/types.ts";
import type { BrowserContext, TestInfo } from "@playwright/test";
import { required } from "../teams/helpers.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
// Completed command rows disappear. Current commands and source records remain.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { test } from "../playwright.ts";

const browserContextsByTest = new WeakMap<TestInfo, Set<BrowserContext>>();
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
  const cache = await mkdtemp(join(tmpdir(), "current-command-history-vite-"));

  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "current-command-history-fixture",
        configureServer(vite) {
          vite.middlewares.use((req, res, next) => {
            if (req.url !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/terminal/current-command-history-fixture.tsx"></script>',
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
    const errors: string[] = [],
      writes: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "GET")
        writes.push(route.request().url());
      await route.fulfill({ json: {} });
    });
    await page.goto(required(server.resolvedUrls?.local[0]));
    await page.waitForFunction(
      () => typeof window.currentCommandHistoryApply === "function",
    );
    const command = (
      id: string,
      state: string,
      type = "commandExecution",
      tool?: string,
    ): Message => ({
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
    const items: Message[] = [
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
    const apply = async (
      label: string,
      rows: Message[],
      enabled: boolean,
      currentTurn?: string,
    ) => {
      await page.evaluate(
        (json) => {
          const snapshot: HistorySnapshot = JSON.parse(json);
          window.currentCommandHistoryApply(snapshot);
        },
        JSON.stringify({
          label,
          items: rows,
          enabled,
          currentTurn,
        }),
      );
      await page
        .locator(`main[data-case="${label}"]`)
        .waitFor({ state: "attached" });
      assert.deepEqual(
        JSON.parse(required(await page.locator("output").textContent())),
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
