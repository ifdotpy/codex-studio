import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { performance } from "node:perf_hooks";
import { spawnFixture, test } from "../playwright.mjs";

test("mutation responses persist one entity in the schema cache within 250 ms", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const state = await mkdtemp(join(tmpdir(), "entity-cache-persist-latency-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), state],
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

    const samples = [];
    for (let index = 0; index < 20; index++) {
      let responseReceivedAt;
      const responsePromise = page.waitForResponse((response) => {
        const url = new URL(response.url());
        if (
          response.request().method() !== "POST" ||
          url.pathname !== "/api/leads"
        )
          return false;
        responseReceivedAt = performance.now();
        return true;
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
      // This DOM update follows the mutation API promise; that promise waits for
      // the entity persister. The subsequent raw readback proves the cache row exists.
      await page.locator(`[data-chat="${body.id}"]`).waitFor();
      const finishedAt = performance.now();
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
        "response row can be read back from the hash-scoped database",
      );
      const elapsedMs = finishedAt - responseReceivedAt;
      samples.push(elapsedMs);
      assert.ok(
        elapsedMs < 250,
        `sample ${index + 1} exceeded persistence bound: ${elapsedMs} ms`,
      );
    }
    const sorted = [...samples].sort((a, b) => a - b);
    console.log(
      "REAL_BROWSER_ENTITY_PERSIST_LATENCY",
      JSON.stringify({
        samples: samples.length,
        responseEntities: 20,
        firstMutationAfterLoadMs: Number(samples[0].toFixed(2)),
        medianMs: Number(sorted[Math.floor(sorted.length / 2)].toFixed(2)),
        maxMs: Number(Math.max(...samples).toFixed(2)),
        samplesMs: samples.map((sample) => Number(sample.toFixed(2))),
      }),
    );
  } finally {
    if (context) await context.close();
    fixture.kill();
  }
});
