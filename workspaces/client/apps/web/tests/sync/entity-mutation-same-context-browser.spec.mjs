import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnFixture, test } from "../playwright.mjs";

test("mutation projections propagate between pages sharing one browser context @sync", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "entity-same-context-"));
  const fixture = spawnFixture(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      state,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, RESOURCE_FRESHNESS_UI_FIXTURE: "1" },
    },
  );
  let log = "";
  let output = "";
  fixture.stdout.on("data", (chunk) => {
    output += String(chunk);
  });
  fixture.stderr.on("data", (chunk) => (log += String(chunk)));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const context = await browser.newContext();
  const pages = [await context.newPage(), await context.newPage()];
  const pulls = [[], []];
  const countPulls = [0, 0];
  const waitFor = async (predicate, message, timeoutMs = 10000) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    assert.fail(message);
  };
  try {
    for (const [index, page] of pages.entries()) {
      page.setDefaultTimeout(12_000);
      page.on("request", (request) => {
        const url = new URL(request.url());
        if (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === "state:entities:v1"
        ) {
          countPulls[index]++;
          pulls[index].push({
            after: url.searchParams.get("after"),
            startedAt: Date.now(),
          });
        }
      });
      page.on("response", async (response) => {
        const url = new URL(response.url());
        if (
          url.pathname !== "/api/sync/pull" ||
          url.searchParams.get("scope") !== "state:entities:v1"
        )
          return;
        try {
          const body = await response.json();
          const pull = pulls[index].at(-1);
          if (pull) pull.documents = body.documents || [];
        } catch {}
      });
      await page.addInitScript(() => {
        const OriginalEventSource = window.EventSource;
        window.__entityPersisterCompletions = [];
        window.__entityMutationDecodes = [];
        const responseJson = Response.prototype.json;
        Response.prototype.json = async function (...args) {
          const value = await responseJson.apply(this, args);
          if (
            this.url &&
            new URL(this.url).pathname === "/api/leads" &&
            value?._syncEntities?.length
          )
            window.__entityMutationDecodes.push({
              at: performance.timeOrigin + performance.now(),
              sequences: value._syncEntities.map((row) => row.seq),
            });
          return value;
        };
        window.__studioSyncEntityPersisterProbe = (value) =>
          window.__entityPersisterCompletions.push(value);
        window.__entityFrames = [];
        window.__delayedResourceFrameDelays = [];
        window.__entityStreamUrls = [];
        window.__delayResourceFrames = false;
        const OriginalBroadcastChannel = window.BroadcastChannel;
        window.BroadcastChannel = class extends OriginalBroadcastChannel {
          set onmessage(listener) {
            if (typeof listener !== "function") {
              super.onmessage = listener;
              return;
            }
            super.onmessage = (event) => {
              if (
                window.__delayResourceFrames &&
                event.data?.kind === "resource-event"
              ) {
                const scheduledAt = performance.now();
                setTimeout(() => {
                  window.__delayedResourceFrameDelays.push(
                    performance.now() - scheduledAt,
                  );
                  listener.call(this, event);
                }, 1800);
              } else listener.call(this, event);
            };
          }
        };
        window.EventSource = class extends OriginalEventSource {
          constructor(...args) {
            super(...args);
            window.__entityStreamUrls.push(String(args[0]));
            const streamIndex = window.__entityStreamUrls.length - 1;
            this.addEventListener("resources", (event) =>
              window.__entityFrames.push({
                streamIndex,
                event: JSON.parse(event.data),
              }),
            );
          }
          addEventListener(type, listener, options) {
            if (type !== "resources")
              return super.addEventListener(type, listener, options);
            const delayed = (event) => {
              if (window.__delayResourceFrames) {
                const scheduledAt = performance.now();
                setTimeout(() => {
                  window.__delayedResourceFrameDelays.push(
                    performance.now() - scheduledAt,
                  );
                  listener.call(this, event);
                }, 1800);
              } else listener.call(this, event);
            };
            return super.addEventListener(type, delayed, options);
          }
        };
      });
      await page.goto(`http://127.0.0.1:${port}`);
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .click();
    }
    await Promise.all(
      pages.map((page) =>
        page.locator("#conversation-title").getByText("Release lead").waitFor(),
      ),
    );
    await waitFor(
      () =>
        pages.every(
          (_, index) =>
            pulls[index].length === countPulls[index] &&
            pulls[index].every((pull) => pull.documents?.length > 0),
        ),
      "initial state pulls did not settle with rows",
    );
    let streamSnapshots = [];
    await waitFor(async () => {
      streamSnapshots = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            streamUrls: window.__entityStreamUrls,
            frames: window.__entityFrames.length,
          })),
        ),
      );
      return streamSnapshots.some(
        (snapshot) => snapshot.streamUrls.length > 0 && snapshot.frames > 0,
      );
    }, "no page received the initial resource stream frame");
    const streamOwnerIndex = streamSnapshots.findIndex(
      (snapshot) => snapshot.streamUrls.length > 0 && snapshot.frames > 0,
    );
    assert.notEqual(
      streamOwnerIndex,
      -1,
      "one same-context page owns the resource stream",
    );
    await pages[1].evaluate(() => {
      window.__delayResourceFrames = true;
    });
    await new Promise((resolve) => setTimeout(resolve, 350));

    const createChat = async (page, pullCountsBeforeMutation) => {
      await page.locator(".project-tree-heading").first().hover();
      const responsePromise = page.waitForResponse(
        (response) =>
          response.request().method() === "POST" &&
          new URL(response.url()).pathname === "/api/leads",
      );
      await page
        .getByRole("button", { name: /^New chat in / })
        .first()
        .click();
      const response = await responsePromise;
      assert.equal(response.status(), 200);
      const body = await response.json();
      assert.ok(
        body._syncEntities?.length,
        "mutation response includes entity rows",
      );
      await page.waitForFunction(
        (sequences) =>
          window.__entityPersisterCompletions.some((completion) =>
            sequences.every((sequence) =>
              completion.sequences.includes(sequence),
            ),
          ),
        body._syncEntities.map((row) => row.seq),
      );
      const completion = await page.evaluate(
        (sequences) =>
          window.__entityPersisterCompletions.find((candidate) =>
            sequences.every((sequence) =>
              candidate.sequences.includes(sequence),
            ),
          ),
        body._syncEntities.map((row) => row.seq),
      );
      const responseDecodedAt = await page.evaluate(
        (sequences) =>
          window.__entityMutationDecodes.find((decode) =>
            sequences.every((sequence) => decode.sequences.includes(sequence)),
          )?.at,
        body._syncEntities.map((row) => row.seq),
      );
      assert.equal(typeof responseDecodedAt, "number");
      if (completion.advanced) {
        assert.equal(
          completion.advanced,
          true,
          `mutation persister advanced the shared checkpoint: ${JSON.stringify(completion)}`,
        );
      } else {
        assert.equal(
          completion.acknowledged,
          true,
          `mutation persister neither advanced nor acknowledged already-covered rows: ${JSON.stringify(completion)}`,
        );
        const highestSequence = Math.max(
          ...body._syncEntities.map((row) => row.seq),
        );
        assert.ok(
          completion.checkpoint >= highestSequence,
          `already-covered checkpoint ${completion.checkpoint} is below response sequence ${highestSequence}`,
        );
        const storedResponseRows = await page.evaluate(
          async (entityIds) => {
            const { db, workspaceId } = await (
              await import("/src/sync/client.ts")
            ).syncDatabase();
            const { API_SCHEMA_HASH } =
              await import("/src/generated/apiSchema.ts");
            const { entityProjectionDatabaseName } =
              await import("/src/sync/entityCacheStorage.ts");
            const stored = await Promise.all(
              entityIds.map(async (id) => {
                const row = await db.projections.findOne(id).exec();
                return row
                  ? { id: row.id, seq: row.seq, payload: row.payload }
                  : null;
              }),
            );
            return {
              databaseName: entityProjectionDatabaseName(
                workspaceId,
                API_SCHEMA_HASH,
              ),
              rows: stored,
            };
          },
          body._syncEntities.map((row) => row.id),
        );
        assert.match(
          storedResponseRows.databaseName,
          /^studio-entity-projection-[a-f0-9]{32}-[a-f0-9]{64}$/,
          "covered rows are read from this workspace's API-schema-keyed projection database",
        );
        assert.deepEqual(
          storedResponseRows.rows,
          body._syncEntities.map(({ id, seq, payload }) => ({
            id,
            seq,
            payload,
          })),
          "already-covered checkpoint is backed by the mutation response rows in the per-hash projection database",
        );
        const coveringPull = pulls.flatMap((tabPulls, tab) =>
          tabPulls.flatMap((pull, ordinal) =>
            body._syncEntities.every((responseRow) =>
              pull.documents?.some(
                (row) =>
                  row.seq === responseRow.seq &&
                  row.payload === responseRow.payload,
              ),
            )
              ? [{ tab, ordinal, after: pull.after }]
              : [],
          ),
        )[0];
        assert.ok(
          coveringPull,
          "an earlier entity pull must account for the already-covered response rows",
        );
        const pullStartedAt =
          pulls[coveringPull.tab][coveringPull.ordinal].startedAt;
        assert.ok(
          pullStartedAt < responseDecodedAt,
          `already-covered pull dispatched after mutation response decode: ${JSON.stringify({ ...coveringPull, pullStartedAt, responseDecodedAt })}`,
        );
        console.log(
          "MUTATION_ROWS_ALREADY_COVERED",
          JSON.stringify({
            ...coveringPull,
            pullStartedAt,
            responseDecodedAt,
            highestSequence,
            pullPredatedMutationSnapshot:
              coveringPull.ordinal < pullCountsBeforeMutation[coveringPull.tab],
          }),
        );
      }
      await page
        .locator("#conversation-title")
        .getByText("New chat", { exact: true })
        .waitFor();
      return body.id;
    };

    // Pull start is timestamped by Playwright and response decode in the
    // renderer; their clocks may have small skew, so compare ordering only.
    const pullsBeforeFirst = [...countPulls];
    const firstId = await createChat(pages[0], pullsBeforeFirst);
    await pages[1].locator(`[data-chat="${firstId}"]`).waitFor();
    await new Promise((resolve) => setTimeout(resolve, 2100));
    assert.ok(
      (await pages[1].evaluate(() => window.__delayedResourceFrameDelays)).some(
        (delay) => delay >= 1750,
      ),
      "observer received its resource frame after the configured delay",
    );
    assert.deepEqual(
      countPulls,
      pullsBeforeFirst,
      "observer uses the shared projection without pulling",
    );

    const pullsBeforeReverse = [...countPulls];
    const secondId = await createChat(pages[1], pullsBeforeReverse);
    await pages[0].locator(`[data-chat="${secondId}"]`).waitFor();
    await new Promise((resolve) => setTimeout(resolve, 2100));
    assert.deepEqual(
      countPulls,
      pullsBeforeReverse,
      "reverse mutation also propagates without pulling",
    );
    assert.ok(
      pulls.every((tabPulls) =>
        tabPulls.every((pull) => pull.documents?.length > 0),
      ),
      "every state pull returns rows",
    );
  } finally {
    await context.close();
    fixture.kill("SIGTERM");
  }
});
