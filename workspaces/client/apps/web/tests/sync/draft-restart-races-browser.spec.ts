import type { Page } from "@playwright/test";
import type { SyncDocument } from "../../src/sync/client.ts";
type DraftPull = { at: number; after: number; docs: SyncDocument[] };
function gate<T>() {
  const started = Promise.withResolvers<T>();
  const released = Promise.withResolvers<void>();
  return {
    startedPromise: started.promise,
    startedResolve: started.resolve,
    release: released.resolve,
    releasePromise: released.promise,
  };
}
type Gate<T> = ReturnType<typeof gate<T>>;
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import { fixtureOrigin } from "./helpers.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { apiSchemaHeaders, syncIdentityFixture, test } from "../playwright.ts";

const workspaceId = "d".repeat(32);

async function createFixture(page: Page) {
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    optimizeDeps: { noDiscovery: true },
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  const remote = new Map<string, SyncDocument>();
  const pulls: DraftPull[] = [];
  const pullCompletions: number[] = [];
  const pushes: { at: number; gate: boolean }[] = [];
  let failPush = false;
  let nextPushGate: Gate<number> | undefined;
  let nextPullGate: Gate<DraftPull> | undefined;
  let nextPullGateRequest: number | undefined;

  server.middlewares.stack.unshift({
    route: "",
    handle(
      req: IncomingMessage,
      res: ServerResponse,
      next: Connect.NextFunction,
    ) {
      const url = new URL(req.url || "/", "http://localhost");
      const json = (value: unknown, status = 200) => {
        res.writeHead(
          status,
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        );
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity")
        json(syncIdentityFixture(workspaceId));
      else if (url.pathname === "/api/sync/pull") {
        const after = Number(url.searchParams.get("after") || 0);
        const limit = Number(url.searchParams.get("limit") || 100);
        const docs = [...remote.values()]
          .filter((row) => row.seq > after)
          .sort((left, right) => left.seq - right.seq)
          .slice(0, limit);
        const checkpoint = {
          seq: docs.reduce((max, row) => Math.max(max, row.seq), after),
        };
        const pull = { at: Date.now(), after, docs };
        pulls.push(pull);
        const gate =
          nextPullGate &&
          (!nextPullGateRequest || pulls.length === nextPullGateRequest)
            ? nextPullGate
            : undefined;
        if (gate) {
          nextPullGate = undefined;
          nextPullGateRequest = undefined;
          gate.startedResolve(pull);
          void gate.releasePromise.then(() => {
            pullCompletions.push(Date.now());
            json({ workspaceId, documents: docs, checkpoint });
          });
          return;
        }
        pullCompletions.push(Date.now());
        json({ workspaceId, documents: docs, checkpoint });
      } else if (url.pathname === "/api/sync/drafts") {
        const gate = nextPushGate;
        pushes.push({ at: Date.now(), gate: Boolean(gate) });
        if (gate) {
          nextPushGate = undefined;
          gate.startedResolve(Date.now());
          void gate.releasePromise.then(() => {
            json([], failPush ? 503 : 200);
          });
          return;
        }
        json([], failPush ? 503 : 200);
      } else next();
    },
  });
  await server.listen();
  await page.addInitScript((id) => {
    localStorage.setItem("codex-sync-workspace", JSON.stringify(id));
    window.draftRestartRacesBrowserFixture__draftReplicationCreations = 0;
    window.draftRestartRacesBrowserFixture__draftReplicationCreationTimes = [];
    window.draftRestartRacesBrowserFixtureTestOnly = {
      onCreate: () => {
        window.draftRestartRacesBrowserFixture__draftReplicationCreations++;
        window.draftRestartRacesBrowserFixture__draftReplicationCreationTimes.push(
          Date.now(),
        );
      },
    };
  }, workspaceId);
  await page.clock.install();
  await page.goto(fixtureOrigin(server) + `/check`);
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    window.draftRestartRacesBrowserFixtureDraftTiming =
      client.DRAFT_SYNC_TIMING_MS;
    window.draftRestartRacesBrowserFixtureDb = await client
      .syncDatabase()
      .then((value) => value.db);
    window.draftRestartRacesBrowserFixtureSyncErrors = [];
    window.draftRestartRacesBrowserFixtureStopDraftSync =
      await client.startDraftReplication(
        (error) => {
          window.draftRestartRacesBrowserFixtureSyncErrors.push(
            error === null ? null : String(error),
          );
        },
        { testOnly: window.draftRestartRacesBrowserFixtureTestOnly },
      );
  });

  const waitFor = async (
    condition: () => unknown | Promise<unknown>,
    message: string,
    timeoutMs = 10_000,
  ) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (await condition()) return;
      await page.waitForTimeout(25);
    }
    throw new Error(message);
  };
  const insertLocal = (id: string) =>
    page.evaluate(
      async (id) =>
        window.draftRestartRacesBrowserFixtureDb.drafts.insert({
          id,
          seq: 0,
          payload: JSON.stringify({ text: id }),
        }),
      id,
    );
  const localText = (id: string) =>
    page.evaluate(async (id) => {
      const doc = await window.draftRestartRacesBrowserFixtureDb.drafts
        .findOne(id)
        .exec();
      return doc ? JSON.parse(doc.payload).text : null;
    }, id);
  const trigger = () =>
    page.evaluate(() => window.dispatchEvent(new Event("online")));
  const countCreations = () =>
    page.evaluate(
      () => window.draftRestartRacesBrowserFixture__draftReplicationCreations,
    );
  const creationTimes = () =>
    page.evaluate(() => [
      ...window.draftRestartRacesBrowserFixture__draftReplicationCreationTimes,
    ]);
  const stop = async () => {
    await page.evaluate(async () =>
      window.draftRestartRacesBrowserFixtureStopDraftSync?.(),
    );
    await server.close();
  };
  return {
    failPush: (value: boolean) => (failPush = value),
    gate,
    insertLocal,
    localText,
    nextPull: (value: Gate<DraftPull>, requestNumber?: number) => {
      nextPullGate = value;
      nextPullGateRequest = requestNumber;
    },
    nextPush: (value: Gate<number>) => (nextPushGate = value),
    page,
    pullCompletions,
    pulls,
    pushes,
    remote,
    server,
    stop,
    trigger,
    countCreations,
    creationTimes,
    waitFor,
  };
}

test("a trigger during the first failing POST survives until the outage is known", async ({
  page,
}) => {
  test.setTimeout(45_000);
  const fx = await createFixture(page);
  try {
    fx.failPush(true);
    const post = fx.gate<number>();
    fx.nextPush(post);
    await fx.insertLocal("n2-local");
    await post.startedPromise;
    fx.remote.set("n2-remote", {
      id: "n2-remote",
      payload: JSON.stringify({ text: "written during first POST" }),
      seq: 1,
    });
    await fx.trigger();
    await page.waitForTimeout(120);
    const creationsBefore = await fx.countCreations();
    post.release();
    await fx.waitFor(
      () => fx.pulls.length > 0 && fx.localText("n2-remote").then(Boolean),
      "the trigger held behind the first POST failure should start a recovery pull",
    );
    assert.equal(await fx.countCreations(), creationsBefore + 1);
    assert.equal(await fx.localText("n2-remote"), "written during first POST");
  } finally {
    await fx.stop();
  }
});

test("a queued trigger is served when the replacement push recovers", async ({
  page,
}) => {
  test.setTimeout(45_000);
  const fx = await createFixture(page);
  try {
    fx.failPush(true);
    await fx.insertLocal("n1-local");
    await fx.waitFor(() => fx.pushes.length > 0, "initial push fails");
    const beforeRestartCreations = await fx.countCreations();
    const replacementPush = fx.gate<number>();
    fx.nextPush(replacementPush);
    const pullsBeforeA = fx.pulls.length;
    await fx.trigger();
    await fx.waitFor(
      () => fx.pulls.length > pullsBeforeA,
      "trigger A starts the replacement pull",
    );
    await replacementPush.startedPromise;
    fx.remote.set("n1-remote", {
      id: "n1-remote",
      payload: JSON.stringify({ text: "written before trigger B" }),
      seq: 1,
    });
    const pullsBeforeB = fx.pulls.length;
    await fx.trigger();
    await page.waitForTimeout(120);
    fx.failPush(false);
    replacementPush.release();
    await fx.waitFor(
      () =>
        fx.pulls.length > pullsBeforeB &&
        fx.localText("n1-remote").then(Boolean),
      "push recovery must reSync the queued trigger",
    );
    assert.equal(await fx.countCreations(), beforeRestartCreations + 1);
    assert.equal(await fx.pulls.length, pullsBeforeB + 1);
    assert.equal(await fx.localText("n1-remote"), "written before trigger B");
  } finally {
    await fx.stop();
  }
});

test("a later trigger in the outage is served at the restart interval", async ({
  page,
}) => {
  test.setTimeout(45_000);
  const fx = await createFixture(page);
  try {
    fx.failPush(true);
    await fx.insertLocal("interval-local");
    await fx.waitFor(() => fx.pushes.length > 0, "push outage starts");
    const pullStart = fx.pulls.length;
    fx.remote.set("interval-a", {
      id: "interval-a",
      payload: JSON.stringify({ text: "remote A" }),
      seq: 1,
    });
    await fx.trigger();
    await fx.waitFor(
      () =>
        fx.pulls.length === pullStart + 1 &&
        fx.localText("interval-a").then(Boolean),
      "trigger A completes its replacement pull",
    );
    const restartAAt = (await fx.creationTimes()).at(-1);
    assert.ok(restartAAt, "trigger A created the first replacement");
    const timing = await page.evaluate(
      () => window.draftRestartRacesBrowserFixtureDraftTiming,
    );
    await page.waitForTimeout(500);
    fx.remote.set("interval-b", {
      id: "interval-b",
      payload: JSON.stringify({ text: "remote B" }),
      seq: 2,
    });
    const pullBDispatchedAt = Date.now();
    await fx.trigger();
    await fx.waitFor(
      () =>
        fx.pulls.length === pullStart + 2 &&
        fx.localText("interval-b").then(Boolean),
      "trigger B gets a deferred pull while push remains failed",
      timing.restartMinInterval + 1_000,
    );
    const lastPull = fx.pulls.at(-1);
    assert.ok(lastPull);
    const pullBAt = lastPull.at;
    assert.ok(
      pullBDispatchedAt - restartAAt < timing.restartMinInterval,
      "trigger B arrived before the minimum restart interval elapsed",
    );
    assert.ok(
      pullBAt >= restartAAt + timing.restartMinInterval,
      `trigger B pull began before the minimum restart interval: A=${restartAAt}, B=${pullBAt}`,
    );
    assert.ok(
      pullBAt <= restartAAt + timing.restartMinInterval + 1_000,
      `trigger B pull exceeded the restart interval plus 1,000 ms: A=${restartAAt}, B=${pullBAt}`,
    );
    assert.equal(await fx.localText("interval-b"), "remote B");
    await page.clock.runFor(3_300);
    assert.equal(
      fx.pulls.length,
      pullStart + 2,
      "the completed trigger B is not polled again during a quiet period",
    );
  } finally {
    await fx.stop();
  }
});

test("repeated triggers wait for a slow initial pull instead of restarting it", async ({
  page,
}) => {
  test.setTimeout(45_000);
  const fx = await createFixture(page);
  try {
    fx.failPush(true);
    await fx.insertLocal("n3-local");
    await fx.waitFor(() => fx.pushes.length > 0, "push outage starts");
    const creationsBefore = await fx.countCreations();
    const slowPull = fx.gate<DraftPull>();
    fx.nextPull(slowPull);
    await fx.trigger();
    const slowPullStarted = await slowPull.startedPromise;
    const pullsAtStart = fx.pulls.length;
    fx.remote.set("n3-remote", {
      id: "n3-remote",
      payload: JSON.stringify({ text: "latest during slow pull" }),
      seq: 1,
    });
    for (let index = 0; index < 9; index++) {
      await fx.trigger();
      await page.waitForTimeout(500);
    }
    assert.equal(fx.pulls.length, pullsAtStart);
    assert.equal(await fx.countCreations(), creationsBefore + 1);
    const measuredHoldMs = Date.now() - slowPullStarted.at;
    assert.ok(
      measuredHoldMs >= 4_000,
      `the initial pull remained held for ${measuredHoldMs} ms`,
    );
    slowPull.release();
    await fx.waitFor(
      () =>
        fx
          .localText("n3-remote")
          .then((value) => value === "latest during slow pull"),
      "the deferred pull applies the newest remote row",
      12_000,
    );
    await page.clock.runFor(250);
    assert.equal(fx.pulls.length, pullsAtStart + 1);
    assert.equal(await fx.countCreations(), creationsBefore + 2);
    assert.equal(await fx.localText("n3-remote"), "latest during slow pull");
  } finally {
    await fx.stop();
  }
});

async function assertOneTriggeredBatchPull(page: Page, failPush: boolean) {
  const fx = await createFixture(page);
  try {
    if (failPush) {
      fx.failPush(true);
      await fx.insertLocal("batch-local");
      await fx.waitFor(() => fx.pushes.length > 0, "push outage starts");
    }
    for (let index = 1; index <= 250; index++) {
      fx.remote.set(`batch-${index}`, {
        id: `batch-${index}`,
        payload: JSON.stringify({ text: `remote ${index}` }),
        seq: index,
      });
    }
    const before = fx.pulls.length;
    await fx.trigger();
    await fx.waitFor(
      async () => {
        const count = await page.evaluate(
          async () =>
            (
              await window.draftRestartRacesBrowserFixtureDb.drafts
                .find()
                .exec()
            ).length,
        );
        return count >= 250 + Number(failPush);
      },
      `one ${failPush ? "restart" : "healthy"} trigger applies all 250 rows`,
    );
    await fx.waitFor(
      () => fx.pulls.length - before === 3,
      "one trigger requests the three 100/100/50 draft pages",
    );
    assert.equal(fx.pulls.length - before, 3);
    assert.equal(
      await page.evaluate(
        async () =>
          (await window.draftRestartRacesBrowserFixtureDb.drafts.find().exec())
            .length,
      ),
      250 + Number(failPush),
      "all server pages are stored locally",
    );
    assert.equal(
      await fx.localText("batch-250"),
      "remote 250",
      "the final page is present in IndexedDB",
    );
    await page.clock.runFor(3_300);
    assert.equal(
      fx.pulls.length - before,
      3,
      "a successful continuation sequence does not poll during the quiet period",
    );
  } finally {
    await fx.stop();
  }
}

test("one trigger pulls every page of a large healthy draft collection", async ({
  page,
}) => {
  test.setTimeout(45_000);
  await assertOneTriggeredBatchPull(page, false);
});

test("one trigger pulls every page during a push outage restart", async ({
  page,
}) => {
  test.setTimeout(45_000);
  await assertOneTriggeredBatchPull(page, true);
});

test("a trigger during a full-page continuation is served after that sequence", async ({
  page,
}) => {
  test.setTimeout(45_000);
  const fx = await createFixture(page);
  try {
    for (let index = 1; index <= 250; index++) {
      fx.remote.set(`continue-${index}`, {
        id: `continue-${index}`,
        payload: JSON.stringify({ text: `page ${index}` }),
        seq: index,
      });
    }
    const secondPage = fx.gate<DraftPull>();
    fx.nextPull(secondPage, 2);
    await fx.trigger();
    await secondPage.startedPromise;
    fx.remote.set("continue-latest", {
      id: "continue-latest",
      payload: JSON.stringify({ text: "written during continuation" }),
      seq: 251,
    });
    await fx.trigger();
    secondPage.release();
    await fx.waitFor(
      () => fx.localText("continue-latest").then(Boolean),
      "the pending trigger after a continuation applies the newest remote row",
    );
    assert.equal(
      await fx.localText("continue-latest"),
      "written during continuation",
    );
    await fx.waitFor(
      () => fx.pulls.length === 4,
      "three pages plus the pending trigger pull",
    );
    assert.equal(fx.pulls.length, 4);
    await page.waitForTimeout(500);
    assert.equal(
      fx.pulls.length,
      4,
      "no extra pull occurs without another trigger",
    );
  } finally {
    await fx.stop();
  }
});

declare global {
  interface Window {
    draftRestartRacesBrowserFixture__draftReplicationCreations: number;
    draftRestartRacesBrowserFixture__draftReplicationCreationTimes: number[];
    draftRestartRacesBrowserFixtureDraftTiming: typeof import("../../src/sync/client.ts").DRAFT_SYNC_TIMING_MS;
    draftRestartRacesBrowserFixtureDb: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >["db"];
    draftRestartRacesBrowserFixtureSyncErrors: (string | null)[];
    draftRestartRacesBrowserFixtureTestOnly: NonNullable<
      NonNullable<
        Parameters<
          typeof import("../../src/sync/client.ts").startDraftReplication
        >[1]
      >["testOnly"]
    >;
    draftRestartRacesBrowserFixtureStopDraftSync: Awaited<
      ReturnType<
        typeof import("../../src/sync/client.ts").startDraftReplication
      >
    >;
  }
}
