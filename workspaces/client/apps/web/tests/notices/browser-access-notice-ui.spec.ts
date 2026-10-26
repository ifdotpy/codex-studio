import type { components } from "../../src/generated/api.ts";
import { fixtureServerOrigin } from "../mobile/browser-fixture.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type { BrowserAccessSnapshot } from "./browser-access-notice-ui.fixture.tsx";
import { test } from "../playwright.ts";
// Render the production browser notice against account-specific desktop responses.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("Browser access notice ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const cache = await mkdtemp(join(tmpdir(), "browser-access-notice-vite-"));
  const screenshot = join(tmpdir(), "codex-browser-access-notice.png");
  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "browser-access-notice-fixture",
        configureServer(vite) {
          vite.middlewares.use((req, res, next) => {
            if (req.url !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/notices/browser-access-notice-ui.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 900, height: 520 });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const requests: (string | null)[] = [];
    await page.route("**/api/desktop?**", async (route) => {
      const url = new URL(route.request().url());
      const key = url.searchParams.get("account_key");
      requests.push(key);
      const browserStatus =
        key === "disabled"
          ? {
              enabled: false,
              reason:
                "The Chrome plugin is disabled in this account's Codex configuration",
            }
          : { enabled: true, reason: null };
      await route.fulfill({
        json: { browser: browserStatus } satisfies Pick<
          components["schemas"]["DesktopResponse"],
          "browser"
        >,
      });
    });
    await page.goto(fixtureServerOrigin(server));
    await page.waitForFunction(
      () => typeof window.applyBrowserAccessSnapshot === "function",
    );
    const notice = page.getByRole("status");
    const set = async (snapshot: BrowserAccessSnapshot) => {
      await page.evaluate(
        (value) => window.applyBrowserAccessSnapshot(value),
        snapshot,
      );
    };
    await set({ accountKey: "disabled", active: true });
    await notice.waitFor();
    const details = notice.locator("details");
    await details.locator("summary").click();
    assert.equal(
      await details.evaluate(
        (element) => element instanceof HTMLDetailsElement && element.open,
      ),
      true,
    );
    assert.match(
      await details.innerText(),
      /The Chrome plugin is disabled in this account's Codex configuration/,
    );
    await page.screenshot({ path: screenshot });
    await set({ accountKey: "default", active: true });
    await page.waitForFunction(
      () => document.querySelector("[role=status]") === null,
    );
    await set({ accountKey: "disabled", active: false });
    await page.waitForFunction(
      () => document.querySelector("[role=status]") === null,
    );
    assert.deepEqual(requests, ["default", "disabled", "default"]);
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ pass: true, cases: 3, screenshot, requests }));
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
