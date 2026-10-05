import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.mjs";

const workspaceId = "d".repeat(32);

async function createFixture(page) {
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  const remote = new Map();
  const pulls = [];
  const pullCompletions = [];
  const pushes = [];
  let failPush = false;
  let nextPushGate;
  let nextPullGate;

  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url || "/", "http://localhost");
      const json = (value, status = 200) => {
        res.writeHead(status, { "Content-Type": "application/json" });
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity") json({ workspaceId });
      else if (url.pathname === "/api/sync/pull") {
        const after = Number(url.searchParams.get("after") || 0);
        const docs = [...remote.values()].filter((row) => row.seq > after);
        const checkpoint = {
          seq: docs.reduce((max, row) => Math.max(max, row.seq), after),
        };
        const pull = { at: Date.now(), after, docs };
        pulls.push(pull);
        const gate = nextPullGate;
        if (gate) {
          nextPullGate = undefined;
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
    window.__draftReplicationCreations = 0;
    window.__codexDraftReplicationTestObserver = {
      onCreate: () => window.__draftReplicationCreations++,
    };
  }, workspaceId);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    window.draftTiming = client.DRAFT_SYNC_TIMING_MS;
    window.db = await client.syncDatabase().then((value) => value.db);
    window.syncErrors = [];
    window.stopDraftSync = await client.startDraftReplication((error) => {
      window.syncErrors.push(error === null ? null : String(error));
    });
  });

  const gate = () => {
    let startedResolve;
    let release;
    const startedPromise = new Promise((resolve) => {
      startedResolve = resolve;
    });
    const releasePromise = new Promise((resolve) => {
      release = resolve;
    });
    return { startedPromise, startedResolve, release, releasePromise };
  };
  const waitFor = async (condition, message, timeoutMs = 10_000) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (await condition()) return;
      await page.waitForTimeout(25);
    }
    throw new Error(message);
  };
  const insertLocal = (id) =>
    page.evaluate(
      async (id) =>
        window.db.drafts.insert({
          id,
          seq: 0,
          payload: JSON.stringify({ text: id }),
        }),
      id,
    );
  const localText = (id) =>
    page.evaluate(async (id) => {
      const doc = await window.db.drafts.findOne(id).exec();
      return doc ? JSON.parse(doc.payload).text : null;
    }, id);
  const trigger = () =>
    page.evaluate(() => window.dispatchEvent(new Event("online")));
  const countCreations = () =>
    page.evaluate(() => window.__draftReplicationCreations);
  const stop = async () => {
    await page.evaluate(async () => window.stopDraftSync?.());
    await server.close();
  };
  return {
    failPush: (value) => (failPush = value),
    gate,
    insertLocal,
    localText,
    nextPull: (value) => (nextPullGate = value),
    nextPush: (value) => (nextPushGate = value),
    page,
    pullCompletions,
    pulls,
    pushes,
    remote,
    server,
    stop,
    trigger,
    countCreations,
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
    const post = fx.gate();
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
    const replacementPush = fx.gate();
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
    const slowPull = fx.gate();
    fx.nextPull(slowPull);
    await fx.trigger();
    await slowPull.startedPromise;
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
    const delay = await page.evaluate(
      () => window.draftTiming.restartMinInterval * 1.5,
    );
    assert.ok(delay > 3_000, "the held pull exceeds the former 3 s wait cap");
    slowPull.release();
    await fx.waitFor(
      () =>
        fx
          .localText("n3-remote")
          .then((value) => value === "latest during slow pull"),
      "the deferred pull applies the newest remote row",
      12_000,
    );
    await page.waitForTimeout(250);
    assert.equal(fx.pulls.length, pullsAtStart + 1);
    assert.equal(await fx.countCreations(), creationsBefore + 2);
    assert.equal(await fx.localText("n3-remote"), "latest during slow pull");
  } finally {
    await fx.stop();
  }
});
