import { listeningPort } from "./helpers.ts";
import type { ViteDevServer } from "vite";
import type { Server } from "node:http";
import type { BrowserContext } from "@playwright/test";
import { required } from "./helpers.ts";
import { assert } from "../chat/assertions.ts";
// Actual queue component, isolated browser storage, no backend or model service.

import {
  createFixtureViteServer,
  FIXTURE_OPTIMIZE_DEPS,
  playwrightScratchRoot,
} from "../playwright.ts";
import { createServer } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { test } from "../playwright.ts";

test("Message Queue Drafts Ui", async ({
  browser: testBrowser,
  context: _testContext,
  page: _testPage,
}, testInfo) => {
  const repo = join(import.meta.dirname, "../../..");

  const createViteServer = createFixtureViteServer;
  const root = await mkdtemp(
    join(playwrightScratchRoot, "message-queue-drafts-"),
  );
  const entryUrl = "/tests/messages/message-queue-drafts-ui.fixture.tsx";
  let vite: ViteDevServer | undefined;
  let browser;
  let server: Server | undefined;
  let quotaContext;
  let context;
  const errors: string[] = [];
  try {
    await writeFile(
      join(root, "index.html"),
      `<div id="root"></div><script type="module" src="${entryUrl}"></script>`,
    );
    vite = await createViteServer({
      optimizeDeps: { ...FIXTURE_OPTIMIZE_DEPS },
      configFile: false,
      root: join(repo, "web"),
      logLevel: "error",
      server: {
        middlewareMode: true,
        hmr: false,
        fs: { allow: [repo, playwrightScratchRoot] },
      },
      define: { "process.env.NODE_ENV": JSON.stringify("test") },
    });
    server = createServer((request, response) => {
      if (request.url === "/") {
        response.setHeader("Content-Type", "text/html");
        response.end(
          `<div id="root"></div><script type="module" src="${entryUrl}"></script>`,
        );
        return;
      }
      required(vite).middlewares(request, response, () => {
        response.statusCode = 404;
        response.end("Not found");
      });
    });
    await new Promise<void>((resolve, reject) => {
      required(server).once("error", reject);
      required(server).listen(0, "127.0.0.1", resolve);
    });
    const url = `http://127.0.0.1:${listeningPort(required(server))}`;
    browser = testBrowser;
    const observe = (context: BrowserContext) => {
      context.setDefaultTimeout(10000);
      context.on("page", (page) =>
        page.on("pageerror", (error) => errors.push(error.message)),
      );
      return context;
    };
    const quota = (quotaContext = observe(await browser.newContext()));
    await quota.addInitScript(() => {
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function (key, value) {
        if (key.startsWith("codex-queue-edit:"))
          throw new DOMException("Full", "QuotaExceededError");
        return original.call(this, key, value);
      };
    });
    const page = await quota.newPage();
    await page.goto(url);
    await page
      .getByRole("button", { name: "Edit queued message 1", exact: true })
      .click();
    await page
      .getByRole("textbox", { name: "Edit queued message", exact: true })
      .fill("Keep this text");
    await page
      .getByRole("alert")
      .filter({ hasText: "could not be saved" })
      .waitFor();
    await page.getByText("Edit not saved", { exact: true }).waitFor();
    await page
      .getByRole("button", { name: "Cancel edit", exact: true })
      .click();
    assert.equal(await page.getByRole("textbox").count(), 0);
    console.log("PASS quota failure is visible and Cancel closes editor");
    await quota.close();
    quotaContext = undefined;
    context = observe(await browser.newContext());
    const first = await context.newPage();
    await first.goto(url);
    await first
      .getByRole("button", { name: "Edit queued message 1", exact: true })
      .click();
    await first.getByRole("textbox").fill("Before reload");
    await first.reload();
    assert.equal(
      await first.getByRole("textbox").inputValue(),
      "Before reload",
    );
    await first.getByRole("textbox").fill("After reload");
    await first
      .getByRole("button", { name: "Save queued message", exact: true })
      .click();
    await first
      .getByRole("button", { name: "Edit queued message 1", exact: true })
      .waitFor();
    assert.equal(
      await first.evaluate(
        () =>
          Object.keys(localStorage).filter((k) =>
            k.startsWith("codex-queue-edit:"),
          ).length,
      ),
      0,
    );
    await first.reload();
    assert.equal(await first.getByRole("textbox").count(), 0);
    console.log("PASS Save removes unchanged recovery ancestors");
    await first
      .getByRole("button", { name: "Edit queued message 1", exact: true })
      .click();
    await first.getByRole("textbox").fill("Tab A original");
    const second = await context.newPage();
    await second.goto(url);
    assert.equal(
      await second.getByRole("textbox").inputValue(),
      "Tab A original",
    );
    await second.getByRole("textbox").fill("Tab B edit");
    await first.getByRole("textbox").fill("Tab A concurrent edit");
    await second
      .getByRole("button", { name: "Save queued message", exact: true })
      .click();
    await second
      .getByRole("button", { name: "Edit queued message 1", exact: true })
      .waitFor();
    const records = await second.evaluate(() =>
      Object.keys(localStorage)
        .filter((k) => k.startsWith("codex-queue-edit:"))
        .map((k) => JSON.parse(localStorage.getItem(k) ?? "null")),
    );
    assert.deepEqual(
      records.map((r) => r.text),
      ["Tab A concurrent edit"],
    );
    await second.reload();
    assert.equal(
      await second.getByRole("textbox").inputValue(),
      "Tab A concurrent edit",
    );
    console.log(
      "PASS concurrent tab edit survives another tab Save and reload",
    );
    await context.close();
    context = undefined;
    assert.deepEqual(errors, [], "The component reports no browser errors");
    console.log(
      JSON.stringify({ browser: browser.browserType().name(), checks: 3 }),
    );
  } catch (error) {
    for (const [index, page] of (
      browser?.contexts().flatMap((context) => context.pages()) || []
    ).entries()) {
      await page
        .screenshot({ path: testInfo.outputPath(`failure-${index}.png`) })
        .catch(() => {});
    }
    console.error(`Failure evidence: ${root}`);
    throw error;
  } finally {
    await quotaContext?.close();
    await context?.close();
    if (server?.listening)
      await new Promise((resolve) => required(server).close(resolve));
    await vite?.close();
    if (!process.env.KEEP_TEST_ARTIFACTS)
      await rm(root, { recursive: true, force: true });
    else console.log(`Evidence: ${root}`);
  }
});
