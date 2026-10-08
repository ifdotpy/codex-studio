import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Production UI and isolated HTTP fixture. Requests deliberately finish out of order.
import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

test("snapshot-order-ui", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const evidence = await mkdtemp(join(tmpdir(), "studio-snapshot-order-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(evidence, "board") },
    },
  );
  let log = "";
  let output = "";
  const fixtureReplies = new Map();
  const fixtureWaiters = new Map();
  fixture.stderr.on("data", (data) => (log += data));
  const waitFor = async (condition, label) => {
    for (let index = 0; index < 120; index++) {
      if (condition()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error(label);
  };
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  fixture.stdout.on("data", (chunk) => {
    output += String(chunk);
    const lines = output.split("\n");
    output = lines.pop() || "";
    for (const line of lines) {
      try {
        const reply = JSON.parse(line);
        const waiter = fixtureWaiters.get(reply.id);
        if (waiter) {
          clearTimeout(waiter.timeout);
          fixtureWaiters.delete(reply.id);
          waiter.resolve(reply);
        } else if (reply.id) fixtureReplies.set(reply.id, reply);
      } catch {}
    }
  });
  const fixtureCommand = (id, params) => {
    const reply = fixtureReplies.has(id)
      ? Promise.resolve(fixtureReplies.get(id))
      : new Promise((resolve, reject) => {
          const timeout = setTimeout(() => {
            fixtureWaiters.delete(id);
            reject(new Error(`Fixture command ${id} timed out: ${log}`));
          }, 8000);
          fixtureWaiters.set(id, { resolve, reject, timeout });
        });
    fixture.stdin.write(
      `${JSON.stringify({ id, method: "fixture/entity-change", params })}\n`,
    );
    return reply;
  };
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const worker = initial.threads.find(
    (agent) => agent.rootId === lead.id && agent.name === "Worker 00",
  );
  const seededWorker = await fixtureCommand("snapshot-revision-0", {
    operation: "rename",
    agent: worker.id,
    name: "Worker revision 0",
  });
  assert.equal(
    seededWorker.ok,
    true,
    "fixture commits the initial worker name",
  );
  assert.ok(Number.isSafeInteger(seededWorker.seq));
  const page = fixturePage;
  await fixturePage.setViewportSize({ width: 1440, height: 960 });
  page.setDefaultTimeout(12000);
  const errors = [],
    requests = [],
    entityPulls = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let revision = 0,
    holdNext = false,
    oldRequest,
    currentFailure = false,
    pendingAnswer;
  await page.route("**/api/sync/pull?*", async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("scope") !== "state:entities:v1")
      return route.fallback();
    const response = await route.fetch();
    const body = await response.json();
    entityPulls.push({
      url: `${url.pathname}${url.search}`,
      checkpoint: body.checkpoint?.seq,
      documents: body.documents ?? [],
    });
    await route.fulfill({ response, json: body });
  });
  await page.route("**/api/session", async (route) => {
    requests.push({ revision, held: holdNext, failure: currentFailure });
    if (holdNext) {
      holdNext = false;
      oldRequest = { route, revision };
      return;
    }
    await route.fulfill(
      currentFailure
        ? { status: 503, json: { error: "Current session unavailable" } }
        : { json: { token: initial.token } },
    );
  });
  await page.route("**/api/messages", async (route) => {
    const fixtureId = `snapshot-revision-${revision}`;
    const committed = await fixtureCommand(fixtureId, {
      operation: "rename",
      agent: worker.id,
      name: `Worker revision ${revision}`,
      status: ["queued", "running", "failed"][revision],
    });
    assert.equal(committed.ok, true, "fixture commits the worker entity");
    assert.ok(
      Number.isSafeInteger(committed.seq),
      "commit has a real sequence",
    );
    return route.fulfill({
      json: { id: route.request().postDataJSON().id, status: "sent" },
    });
  });
  await page.route("**/api/answer", async (route) => {
    pendingAnswer = route;
  });
  await page.addInitScript(() => {
    const NativeEventSource = window.EventSource;
    window.__snapshotOrderResourceFrames = [];
    window.EventSource = new Proxy(NativeEventSource, {
      construct(Target, args) {
        const stream = new Target(...args);
        stream.addEventListener("resources", (event) => {
          window.__snapshotOrderResourceFrames.push({
            at: performance.timeOrigin + performance.now(),
            frame: JSON.parse(event.data),
          });
        });
        return stream;
      },
    });
  });
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator("#team-toggle").click();
  const workerButton = page.locator(`[data-worker="${worker.id}"]`);
  await workerButton.getByText("Worker revision 0", { exact: true }).waitFor();

  // The real Mantine loading state must not translate the button label.
  const card = page.locator('[data-request="async-question"]');
  await card.getByRole("button", { name: "Answer", exact: true }).click();
  await card
    .getByRole("textbox", { name: "Which scope?" })
    .fill("Only this fixture");
  const answer = card.getByRole("button", {
    name: "Send answer",
    exact: true,
  });
  const label = answer.locator(".mantine-Button-inner");
  // Finish the browser scroll before measuring the loading transition.
  await answer.scrollIntoViewIfNeeded();
  const before = await label.boundingBox();
  await answer.click();
  await waitFor(() => pendingAnswer, "Answer request reaches the held route");
  await page.waitForFunction(() =>
    document.querySelector(
      '.request-answer-actions button[data-loading="true"]',
    ),
  );
  assert.equal(
    await label.evaluate((node) => getComputedStyle(node).transform),
    "none",
    "Loading keeps the inner button transform unset",
  );
  const during = await label.boundingBox();

  for (const key of ["x", "y", "width", "height"])
    assert.ok(
      Math.abs(before[key] - during[key]) <= 1,
      `Loading label ${key} stays stable`,
    );
  const requestId = pendingAnswer.request().postDataJSON().id;
  const serverAnswer = await pendingAnswer.fetch();
  const answerResponse = await serverAnswer.json();
  assert.equal(serverAnswer.status(), 200);
  assert.equal(answerResponse.status, "answered");
  const serverTombstone = answerResponse._syncEntities?.find(
    (document) => document.id === `entity:request:${requestId}`,
  );
  assert.ok(
    serverTombstone?._deleted,
    "The server answer returns the request tombstone",
  );
  assert.equal(
    serverTombstone.payload,
    JSON.stringify({ collection: "request", id: requestId, value: {} }),
  );
  await pendingAnswer.fulfill({ response: serverAnswer, json: answerResponse });
  await card.waitFor({ state: "hidden" });
  assert.ok(
    answerResponse._syncEntities.some(
      (document) => document.id === serverTombstone.id && document._deleted,
    ),
  );
  assert.equal(
    entityPulls.some((pull) =>
      pull.documents.some(
        (document) => document.id === serverTombstone.id && document._deleted,
      ),
    ),
    false,
    "The answer tombstone is applied once from the mutation response, not redelivered by a pull",
  );

  for (const oldFailure of [false, true]) {
    oldRequest = null;
    holdNext = true;
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await waitFor(() => oldRequest, "Background snapshot poll is held");
    const oldRevision = revision;
    revision++;
    const newerSession = page.waitForResponse(
      (response) =>
        new URL(response.url()).pathname === "/api/session" && response.ok(),
    );
    await page.locator("#message").fill(`Snapshot refresh ${revision}`);
    await page.locator("#send").click();
    try {
      await workerButton
        .getByText(`Worker revision ${revision}`, { exact: true })
        .waitFor();
    } catch (error) {
      const resourceFrames = await page.evaluate(
        () => window.__snapshotOrderResourceFrames,
      );
      const workerPulls = entityPulls.map((pull) => ({
        url: pull.url,
        checkpoint: pull.checkpoint,
        workerRows: pull.documents.flatMap((document) => {
          if (document.id !== `entity:agent:${worker.id}`) return [];
          const envelope = JSON.parse(document.payload);
          return [
            {
              seq: document.seq,
              deleted: document._deleted,
              name: envelope.value.name,
              status: envelope.value.status,
            },
          ];
        }),
      }));
      console.error(
        "SNAPSHOT_ORDER_SYNC",
        JSON.stringify({ revision, resourceFrames, workerPulls }),
      );
      throw error;
    }
    await waitFor(
      () =>
        entityPulls.some((pull) =>
          pull.documents.some((document) => {
            if (document.id !== `entity:agent:${worker.id}`) return false;
            return (
              JSON.parse(document.payload).value.name ===
              `Worker revision ${revision}`
            );
          }),
        ),
      "A real entity pull returns the committed worker revision",
    );
    const workerRows = entityPulls.flatMap((pull) =>
      pull.documents.flatMap((document) => {
        if (document.id !== `entity:agent:${worker.id}`) return [];
        const value = JSON.parse(document.payload).value;
        return [{ seq: document.seq, name: value.name }];
      }),
    );
    const initialWorkerSequence = workerRows.find(
      (row) => row.name === "Worker revision 0",
    )?.seq;
    const committedWorkerSequence = workerRows.findLast(
      (row) => row.name === `Worker revision ${revision}`,
    )?.seq;
    assert.ok(
      Number.isSafeInteger(initialWorkerSequence) &&
        Number.isSafeInteger(committedWorkerSequence) &&
        committedWorkerSequence > initialWorkerSequence,
      "The fixture write advances the real worker entity sequence",
    );
    await newerSession;
    await waitFor(
      () =>
        requests.some(
          (request) => request.revision === revision && !request.held,
        ),
      "Newer session request completes before the held response",
    );
    const expectedStatus = revision === 1 ? "Working" : "Failed";
    assert.equal(
      await workerButton.locator("small").innerText(),
      expectedStatus,
    );
    const freshNode = await workerButton.elementHandle();
    assert.equal(
      oldRequest.revision,
      oldRevision,
      "Held credential request started before the worker entity changed",
    );
    await oldRequest.route.fulfill(
      oldFailure
        ? { status: 503, json: { error: "Obsolete snapshot failure" } }
        : { json: { token: initial.token } },
    );
    // Two paints settle the response before the next scheduled 1.6s poll can hide a regression.
    await page.waitForTimeout(150);
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    assert.equal(
      await workerButton.locator("strong").innerText(),
      `Worker revision ${revision}`,
      "Old responses cannot replace the confirmed worker",
    );
    assert.equal(
      await workerButton.locator("small").innerText(),
      expectedStatus,
      "Old responses cannot revert worker status",
    );
    assert.equal(
      await freshNode.evaluate((node) => node.isConnected),
      true,
      "Obsolete responses do not regroup the confirmed worker",
    );
    assert.equal(
      await page.locator("#error").count(),
      0,
      "An obsolete error does not replace a successful snapshot",
    );
  }

  // A current failure still appears and the next current success clears it.
  currentFailure = true;
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page
    .locator("#error")
    .getByText("Current session unavailable", { exact: true })
    .waitFor();
  currentFailure = false;
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page.locator("#error").waitFor({ state: "hidden" });
  expect(errors).toEqual([]);
  await page.screenshot({
    path: join(evidence, "snapshot-order.png"),
    fullPage: true,
  });
  await writeFile(
    join(evidence, "requests.json"),
    JSON.stringify({ requests, button: { before, during } }, null, 2),
  );
  console.log(
    `PASS: delayed credential success and error cannot revert newer replicated data; current errors remain visible; loading button label stays fixed. ${evidence}`,
  );
});
