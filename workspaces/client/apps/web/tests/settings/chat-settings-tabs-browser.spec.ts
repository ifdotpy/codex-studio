import type { components } from "../../src/generated/api.ts";
import type {} from "./chat-settings-tabs-browser.fixture.tsx";
import { fixtureUrl } from "./fixture-helpers.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
  test,
  expect,
  createFixtureViteServer,
} from "../playwright.ts";

test("Chat settings is one compact Team panel with account eligibility and no tabs", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "execution-tabs-"));
  const server = await createFixtureViteServer({
    root,
    configFile: false,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "execution-tabs",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/settings/chat-settings-tabs-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  await server.listen();
  try {
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
        json: {
          workspaceId: "1234567890abcdef1234567890abcdef",
          syncProtocol: 2,
        } satisfies components["schemas"]["SyncIdentityResponse"],
      }),
    );
    await page.route("**/api/sync/stream?**", (route) => {
      const resources = JSON.parse(
        new URL(route.request().url()).searchParams.get("resources") || "[]",
      );
      return route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(
          `event: resources\ndata: ${JSON.stringify({ protocol: 3, workspaceId: "1234567890abcdef1234567890abcdef", epoch: "fixture", revision: 1, reason: "initial", resources })}\n\n`,
        ),
      });
    });
    const url = fixtureUrl(server, "check");
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(url);
    const modal = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    await expect(modal).toBeVisible();
    await expect(modal.getByRole("tablist")).toHaveCount(0);
    await expect(modal.getByRole("listbox")).toHaveCount(0);
    await expect(
      modal.getByRole("radiogroup", { name: "Agent role" }),
    ).toHaveCount(0);
    await modal
      .getByRole("combobox", { name: "Delegation depth", exact: true })
      .selectOption("2");
    await page.waitForFunction(() =>
      window.chatSettingsFixture.calls.some(
        (call) => call.worker_defaults?.delegation_depth === 2,
      ),
    );
    await modal
      .getByRole("textbox", { name: "Subagent parallelism", exact: true })
      .fill("0");
    await modal.getByRole("button", { name: "Apply", exact: true }).click();
    await page.waitForFunction(() =>
      window.chatSettingsFixture.calls.some(
        (call) => call.subagent_concurrency === 0,
      ),
    );
    await expect(
      modal.getByRole("checkbox", { name: "Work 2", exact: true }),
    ).toBeVisible();
    await modal.getByRole("checkbox", { name: "Work 2", exact: true }).click();
    await page.waitForFunction(() =>
      window.chatSettingsFixture.calls.some((call) =>
        call.excluded_account_keys?.includes("account-1"),
      ),
    );
    await expect(modal.locator(".execution-accounts")).toContainText(
      "4 of 5 enabled",
    );
    await expect(
      modal.getByRole("checkbox", { name: "Work 1", exact: true }),
    ).toBeDisabled();
    await expect(modal.getByRole("tab", { name: /^Access/ })).toHaveCount(0);
    await expect(modal.locator(".execution-browser")).toHaveCount(0);
    await expect(
      modal.getByRole("radiogroup", {
        name: "Full access without approval",
        exact: true,
      }),
    ).toBeVisible();
    await expect(modal.getByRole("dialog")).toHaveCount(0);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
