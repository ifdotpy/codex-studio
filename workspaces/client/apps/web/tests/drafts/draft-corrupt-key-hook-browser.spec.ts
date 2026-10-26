import type { components } from "../../src/generated/api.ts";
import { fixtureOrigin } from "../drafts/draft-fixture-helpers.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.ts";

test("malformed current-draft key does not crash hook initialization", async ({
  browser,
}) => {
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.use("/draft-corrupt-check", (_request, response) => {
    response.setHeader("Content-Type", "text/html");
    response.end("<!doctype html><title>Corrupt draft key</title>");
  });
  await server.listen();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const workspaceId = "a".repeat(32);
    await context.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId } satisfies Pick<
          components["schemas"]["SyncIdentityResponse"],
          "workspaceId"
        >,
      }),
    );
    await context.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: {
          workspaceId,
          documents: [],
          checkpoint: { seq: 0 },
        } satisfies Pick<
          components["schemas"]["SyncPullResponse"],
          "workspaceId" | "documents" | "checkpoint"
        >,
      }),
    );
    const origin = fixtureOrigin(server);
    await page.goto(`${origin}/draft-corrupt-check`);
    await page.evaluate((workspaceId) => {
      localStorage.setItem("codex-sync-workspace", JSON.stringify(workspaceId));
      localStorage.setItem(
        `codex-chat-draft:${workspaceId}:healthy`,
        JSON.stringify({
          version: 1,
          workspace: workspaceId,
          session: "healthy",
          text: "Healthy chat remains visible",
          deleted: false,
          source: "local",
        }),
      );
      localStorage.setItem(`codex-chat-draft:${workspaceId}:%E0%A4%A`, "{}");
    }, workspaceId);
    await page.evaluate(async () => {
      const { mount } =
        await import("/src/../tests/drafts/draft-corrupt-key-hook-browser.fixture.tsx");
      await mount();
    });
    await page.waitForFunction(
      () =>
        window.corruptDraft_draft?.drafts.healthy ===
        "Healthy chat remains visible",
    );
    assert.match(
      await page.evaluate(() => window.corruptDraft_draft.error),
      /saved drafts could not be read/i,
    );
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
    await server.close();
  }
});
