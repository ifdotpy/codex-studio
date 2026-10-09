import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, spawnFixture, test } from "../playwright.mjs";

test("mutation response persistence completes before its UI update", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "entity-cache-persist-latency-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      state,
    ],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => (fixtureLog += String(chunk)));
  let context;
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const identity = await fetch(`${origin}/api/sync/identity`).then(
      (response) => response.json(),
    );
    context = await browser.newContext();
    const page = await context.newPage();
    await page.goto(origin);
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .waitFor();
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await page.locator("#message").waitFor();

    for (let index = 0; index < 20; index++) {
      const responsePromise = page.waitForResponse((response) => {
        const url = new URL(response.url());
        return (
          response.request().method() === "POST" &&
          url.pathname === "/api/leads"
        );
      });
      await page.locator(".project-tree-heading").first().hover();
      await page
        .getByRole("button", { name: /^New chat in / })
        .first()
        .click();
      const response = await responsePromise;
      const body = await response.json();
      assert.equal(response.status(), 200);
      const entities = body._syncEntities || [];
      assert.ok(
        entities.length > 0,
        "mutation response includes _syncEntities",
      );
      const entity = entities.find((row) => row.id.startsWith("entity:"));
      assert.ok(entity, "mutation response carries a persisted entity row");
      const persisted = await page.evaluate(
        async ({ workspaceId, id, seq }) => {
          const names = (await indexedDB.databases()).map(({ name }) => name);
          const databaseName = names.find(
            (name) =>
              name?.includes(workspaceId) && name.endsWith("--0--projections"),
          );
          if (!databaseName)
            throw new Error("Projection database was not opened");
          const database = await new Promise((resolve, reject) => {
            const request = indexedDB.open(databaseName);
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
          });
          try {
            return await new Promise((resolve, reject) => {
              const transaction = database.transaction("docs", "readonly");
              const request = transaction.objectStore("docs").get(id);
              request.onsuccess = () => resolve(request.result?.seq >= seq);
              request.onerror = () => reject(request.error);
            });
          } finally {
            database.close();
          }
        },
        { workspaceId: identity.workspaceId, id: entity.id, seq: entity.seq },
      );
      assert.equal(
        persisted,
        true,
        "response row is readable from the hash-scoped database before post resolves",
      );
      // The rendered chat is created only after the API post promise resolves;
      // api.test covers that promise's 250 ms persister bound with fake timers.
      // This browser check asserts ordering, without claiming a wall-clock SLA.
      const chat = page.locator(`[data-chat="${body.id}"]`);
      await chat.waitFor();
      await expect(chat).toBeVisible();
      assert.ok(
        await chat.innerText(),
        "the new value is rendered after post resolves",
      );
    }
    console.log(
      "REAL_BROWSER_ENTITY_PERSIST_ORDERING",
      JSON.stringify({
        samples: 20,
        responseEntities: 20,
        cacheReadBackBeforeUi: true,
      }),
    );
  } finally {
    if (context) await context.close();
    fixture.kill();
  }
});
