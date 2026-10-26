import { listeningPort } from "./helpers.ts";
import { required } from "./helpers.ts";
import type {} from "./completed-markdown-cache-browser.fixture.tsx";
import assert from "node:assert/strict";
import { join } from "node:path";
import { test, expect, createFixtureViteServer } from "../playwright.ts";

test("Completed Markdown Cache Browser", async ({ page }) => {
  const root = join(import.meta.dirname, "../../");
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "completed-markdown-cache-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/messages/completed-markdown-cache-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const errors: string[] = [],
      requests: Array<{ path: string; agent: string | null; method: string }> =
        [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      console.error("Browser error:", error.message);
    });
    await page.route("**/api/**", (route) => {
      const url = new URL(route.request().url());
      requests.push({
        path: url.pathname,
        agent: url.searchParams.get("agent"),
        method: route.request().method(),
      });
      return route.fulfill({
        json: {
          name: "report.md",
          mime: "text/plain",
          base64: Buffer.from("Current workspace file.").toString("base64"),
        } satisfies import("../../src/api.ts").GetResult<"/api/file">,
      });
    });
    await page.goto(
      `http://127.0.0.1:${listeningPort(required(server.httpServer))}/check`,
      { waitUntil: "commit" },
    );
    await expect(page.locator("article a")).toHaveCount(2);
    assert.equal(await page.evaluate(() => window.markdownCompilations), 2);
    await page.evaluate(() => window.markdownChat(true));
    await expect(page.locator("article").first()).toContainText("Other chat");
    await expect
      .poll(() => page.evaluate(() => window.markdownCompilations))
      .toBe(4);
    await page.evaluate(() => {
      window.markdownHide();
      window.markdownChat(false);
      window.markdownCompilations = 0;
      window.markdownShow();
    });
    await expect(page.locator("article a")).toHaveCount(2);
    assert.equal(
      await page.evaluate(() => window.markdownCompilations),
      0,
      "Completed exact text survives component remount without parsing again",
    );
    await page.evaluate(() =>
      window.markdownText("[Changed report][r].\n\n[r]: ./changed.md"),
    );
    await expect(
      page.getByRole("link", { name: "Changed report" }),
    ).toBeVisible();
    assert.equal(await page.evaluate(() => window.markdownCompilations), 1);
    await page.evaluate(() => {
      window.markdownHide();
      window.markdownAgent("second");
      window.markdownShow();
    });
    await page.getByRole("link", { name: "Changed report" }).click();
    await page.getByText("Current workspace file.", { exact: true }).waitFor();
    assert.equal(
      required(requests.at(-1)).agent,
      "second",
      "Cached links use the current agent",
    );
    await page.keyboard.press("Escape");
    await page.evaluate(() =>
      window.markdownText("[Remote](file://remote/report.md)"),
    );
    for (let attempt = 0; attempt < 2; attempt++) {
      const trigger = page.getByRole("link", { name: "Remote" });
      await trigger.focus();
      await trigger.press("Enter");
      await page.getByRole("dialog", { name: "Cannot open file" }).waitFor();
      await page
        .getByText("Remote file links are not supported.", { exact: true })
        .waitFor();
      await page.keyboard.press("Escape");
      await page.getByRole("dialog").waitFor({ state: "hidden" });
      await page.waitForFunction(
        () => document.activeElement?.textContent === "Remote",
        null,
        { timeout: 3000 },
      );
    }
    await page.evaluate(() => window.markdownResults());
    for (let attempt = 0; attempt < 2; attempt++) {
      const trigger = page.getByRole("button", { name: "file.txt Patch" });
      await trigger.focus();
      await trigger.press("Enter");
      await page
        .getByRole("dialog", { name: "file.txt", exact: true })
        .waitFor();
      await page.keyboard.press("Escape");
      await page.getByRole("dialog").waitFor({ state: "hidden" });
      await page.waitForFunction(
        () => document.activeElement?.textContent === "file.txtPatch",
        null,
        { timeout: 3000 },
      );
    }
    const streamingText = "Streaming-only **prefix**.";
    await page.evaluate(
      (text) => window.markdownText(text, true),
      streamingText,
    );
    await expect(page.locator("article strong")).toHaveText("prefix");
    await page.evaluate(() => {
      window.markdownHide();
      window.markdownShow();
    });
    await expect(page.locator("article strong")).toHaveText("prefix");
    assert.equal(
      await page.evaluate(
        (text) => window.markdownCache.get(text),
        streamingText,
      ),
      undefined,
      "Streaming-only text is never shared in the completed cache",
    );
    await page.evaluate(() =>
      window.markdownText(
        'Safe <img src="bad" onerror="window.executed=true"> [unsafe](javascript:alert(1))',
      ),
    );
    await expect(page.locator("article .markdown-block p")).toContainText(
      "Safe",
    );
    assert.equal(
      await page
        .locator('.prose [onerror],.prose a[href^="javascript:"]')
        .count(),
      0,
    );
    assert.deepEqual(errors, []);
    assert.ok(requests.every((request) => request.method === "GET"));
    console.log(
      "PASS real WASM Markdown cache, exact text, current workspace, dialog focus, sanitizer and streaming exclusion",
    );
  } finally {
    await server.close();
  }
});
