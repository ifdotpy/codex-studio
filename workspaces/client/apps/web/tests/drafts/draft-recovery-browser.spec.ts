import type { AddressInfo } from "node:net";
import type { components } from "../../src/generated/api.ts";
import { requestBody } from "../messages/helpers.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.ts";

test("Draft recovery", async ({ context: testContext }) => {
  test.setTimeout(180_000);
  // Unsent input stays in memory and is discarded on reload.
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = await testContext.newPage(),
      errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const workspaceId = "b".repeat(32);
    const pushes: PostBody<"/api/sync/drafts">["rows"] = [];
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Draft recovery</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId } satisfies Pick<
          components["schemas"]["SyncIdentityResponse"],
          "workspaceId"
        >,
      }),
    );
    await page.route("**/api/sync/drafts", (route) => {
      pushes.push(...requestBody<PostBody<"/api/sync/drafts">>(route).rows);
      return route.fulfill({ json: [] });
    });
    await page.route("**/api/sync/pull?*", (route) =>
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
    const origin = `http://127.0.0.1:${(server.httpServer!.address() as AddressInfo).port}`;
    const mount = async (fail: boolean) => {
      await page.goto(origin + "/check");
      await page.evaluate(async (fail: boolean) => {
        const { mount } =
          await import("/src/../tests/drafts/draft-recovery-browser.fixture.tsx");
        await mount(fail);
      }, fail);
      await page.waitForFunction(() => !!window.draft);
    };
    const edit = (text: string) =>
      page.evaluate(
        (text: string) =>
          window.draft.setDrafts((old) => ({
            ...old,
            lead: text,
          })),
        text,
      );
    await mount(true);
    await edit("Unsent draft");
    await page.waitForFunction(
      () => window.draft.drafts.lead === "Unsent draft",
    );
    assert.equal(
      await page.evaluate(() =>
        Object.keys(localStorage).some(
          (key) =>
            key.startsWith("codex-chat-draft:") || key.includes(":pending:"),
        ),
      ),
      false,
    );
    await mount(false);
    await page.waitForTimeout(500);
    assert.notEqual(
      await page.evaluate(() => window.draft.drafts.lead),
      "Unsent draft",
    );
    assert.deepEqual(errors, []);
  } finally {
    await server.close();
  }
});
