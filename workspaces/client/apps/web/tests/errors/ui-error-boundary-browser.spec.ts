import { required } from "../teams/helpers.ts";
import { createServer } from "vite";
import type {} from "./ui-error-boundary-browser.fixture.tsx";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.ts";

test("ui-error-boundary-browser", async ({ page }) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const cache = await mkdtemp(join(tmpdir(), "studio-display-boundary-"));

  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "display-boundary-fixture",
        configureServer(vite) {
          vite.middlewares.use((req, res, next) => {
            if (req.url !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/errors/ui-error-boundary-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const useWebkit = process.env.BROWSER === "webkit";
    const writes: string[] = [];
    page.on("request", (request) => {
      if (request.method() !== "GET") writes.push(request.url());
    });
    await page.goto(required(server.resolvedUrls?.local[0]));
    await page.getByText("Feature ready").waitFor();
    await page
      .getByRole("textbox", { name: "Draft" })
      .fill("Unsent input remains");
    await page.evaluate(() =>
      window.uiErrorBoundaryBrowserFixture.setBad(true),
    );
    await page
      .getByRole("alert", { name: "this conversation display error" })
      .waitFor();
    assert.equal(
      await page.getByRole("textbox", { name: "Draft" }).inputValue(),
      "Unsent input remains",
    );
    await page.getByRole("button", { name: "Try again" }).click();
    await page.getByRole("alert").waitFor();
    await page.getByRole("button", { name: "Other chat" }).click();
    await page.getByText("Feature ready").waitFor();
    assert.equal(await page.getByRole("alert").count(), 0);
    assert.equal(
      await page.getByRole("textbox", { name: "Draft" }).inputValue(),
      "Unsent input remains",
    );
    await page.evaluate(() =>
      window.uiErrorBoundaryBrowserFixture.setBad(true),
    );
    await page.getByRole("alert").waitFor();
    await page.evaluate(() =>
      window.uiErrorBoundaryBrowserFixture.setBad(false),
    );
    await page.getByRole("button", { name: "Try again" }).click();
    await page.getByText("Feature ready").waitFor();
    await page.evaluate(() => {
      localStorage.setItem("retained-draft", "durable input");
      window.uiErrorBoundaryBrowserFixture.setFatal(true);
    });
    await page.getByRole("alert", { name: "Studio display error" }).waitFor();
    await page.getByRole("button", { name: "Reload Studio" }).click();
    await page.getByText("Feature ready").waitFor();
    assert.equal(
      await page.evaluate(() => localStorage.getItem("retained-draft")),
      "durable input",
    );
    expect(writes).toEqual([]);
    console.log(
      `PASS (${useWebkit ? "WebKit" : "Chromium"}): scoped failure preserves navigation and input, chat switch/retry recover, root fallback reload preserves storage, no writes`,
    );
  } finally {
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
