import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
  installApiSchemaFixtureHeaders,
} from "../playwright.mjs";
// Production UI and isolated HTTP fixture. Requests deliberately finish out of order.
import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

async function runSnapshotOrderUi(
  fixturePage,
  { checkRequestAnchor = false } = {},
) {
  test.setTimeout(45_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-snapshot-order-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      evidence,
    ],
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
      const result = await condition();
      if (result) return result;
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
  installApiSchemaFixtureHeaders(page);
  const errors = [],
    requests = [],
    entityPulls = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let revision = 0,
    holdNext = false,
    oldRequest,
    currentFailure = false,
    pendingAnswer;
  const mutationSequences = new Map();
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
    mutationSequences.set(revision, committed.seq);
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
  const answer = card.getByRole("button", {
    name: "Send answer",
    exact: true,
  });
  const label = answer.locator(".mantine-Button-inner");
  const answerForm = card.locator(".request-answer-form");
  await answerForm.waitFor({ state: "visible" });
  await page.waitForFunction(
    () => {
      const root = document.querySelector("#messages");
      const card = document.querySelector('[data-request="async-question"]');
      const form = card?.querySelector(".request-answer-form");
      const label = card?.querySelector(
        ".request-answer-actions .mantine-Button-inner",
      );
      if (
        !root ||
        !card ||
        !form ||
        !label ||
        root.querySelectorAll("[data-message]").length === 0
      )
        return false;
      const rect = (node) => {
        const { x, y, width, height } = node.getBoundingClientRect();
        return [x, y, width, height];
      };
      const current = [
        root.scrollTop,
        root.scrollHeight,
        ...rect(card),
        ...rect(form),
        ...rect(label),
      ];
      const state = (window.__snapshotOrderOpenStable ??= {
        previous: null,
        frames: 0,
      });
      state.frames =
        state.previous &&
        current.every(
          (value, index) => Math.abs(value - state.previous[index]) < 0.1,
        )
          ? state.frames + 1
          : 0;
      state.previous = current;
      return state.frames >= 2;
    },
    undefined,
    { polling: "raf", timeout: 10000 },
  );
  if (checkRequestAnchor) {
    const position = await page.evaluate(() => {
      const root = document.querySelector("#messages");
      const card = document.querySelector('[data-request="async-question"]');
      const form = card?.querySelector(".request-answer-form");
      if (!root || !card || !form)
        throw new Error("Answer form is not mounted");
      const rootRect = root.getBoundingClientRect();
      const cardRect = card.getBoundingClientRect();
      const formRect = form.getBoundingClientRect();
      return {
        scrollTop: root.scrollTop,
        root: {
          top: rootRect.top,
          bottom: rootRect.bottom,
          height: root.clientHeight,
        },
        card: { top: cardRect.top, bottom: cardRect.bottom },
        form: { top: formRect.top, bottom: formRect.bottom },
        topOffset: cardRect.top - rootRect.top,
        formBottomGap: rootRect.bottom - formRect.bottom,
      };
    });
    test.fail(
      true,
      "Opening an answer should anchor the request card 12px below the transcript viewport top; the settled card misses that anchor and clips the reply form.",
    );
    assert.ok(
      Math.abs(position.topOffset - 12) <= 1 && position.formBottomGap >= 0,
      `Opened request remains anchored with its full reply form visible: ${JSON.stringify(position)}`,
    );
    return;
  }
  await card
    .getByRole("textbox", { name: "Which scope?" })
    .fill("Only this fixture");
  await page.evaluate(() => {
    window.__snapshotOrderOpenStable = { previous: null, frames: 0 };
  });
  // Record every frame around the transition so a geometry failure tells us
  // whether the answer fields, preceding content, or transcript scroll moved.
  await page.evaluate(() => {
    window.__snapshotOrderLabelFrames = {
      phase: "scrolling",
      stableFrames: 0,
      previous: null,
      frames: [],
    };
    const sample = () => {
      const record = window.__snapshotOrderLabelFrames;
      const label = document.querySelector(
        '[data-request="async-question"] .request-answer-actions .mantine-Button-inner',
      );
      const card = document.querySelector('[data-request="async-question"]');
      const form = card?.querySelector(".request-answer-form");
      const fields = form?.querySelector(".request-answer-fields");
      const actions = form?.querySelector(".request-answer-actions");
      if (!record || !label || !card || !form || !fields || !actions) return;
      const rect = (element) => {
        const bounds = element.getBoundingClientRect();
        return {
          y: bounds.y,
          width: bounds.width,
          height: bounds.height,
          className:
            typeof element.className === "string" ? element.className : "",
        };
      };
      const priorSiblings = [];
      for (
        let node = actions;
        node && node !== card;
        node = node.parentElement
      ) {
        for (
          let sibling = node.previousElementSibling;
          sibling;
          sibling = sibling.previousElementSibling
        )
          priorSiblings.push(rect(sibling));
      }
      const current = {
        phase: record.phase,
        label: rect(label),
        fields: rect(fields),
        actions: rect(actions),
        form: rect(form),
        card: rect(card),
        transcriptScrollTop: document.querySelector("#messages")?.scrollTop,
        priorSiblings,
      };
      if (
        record.previous &&
        Math.abs(record.previous.label.y - current.label.y) < 0.1 &&
        Math.abs(record.previous.label.height - current.label.height) < 0.1 &&
        Math.abs(record.previous.label.width - current.label.width) < 0.1
      )
        record.stableFrames += 1;
      else record.stableFrames = 0;
      record.previous = current;
      record.frames.push(current);
      if (record.frames.length > 100) record.frames.shift();
    };
    const tick = () => {
      const record = window.__snapshotOrderLabelFrames;
      if (!record || record.phase === "done") return;
      sample();
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
  // Baseline only after the open transition and filled form are geometrically stable.
  await page.waitForFunction(
    () => {
      const root = document.querySelector("#messages");
      const label = document.querySelector(
        '[data-request="async-question"] .request-answer-actions .mantine-Button-inner',
      );
      const form = document.querySelector(
        '[data-request="async-question"] .request-answer-form',
      );
      if (
        !root ||
        !label ||
        !form ||
        root.querySelectorAll("[data-message]").length === 0
      )
        return false;
      const formRect = form.getBoundingClientRect();
      const rootRect = root.getBoundingClientRect();
      if (
        formRect.height <= 0 ||
        formRect.bottom <= rootRect.top ||
        formRect.top >= rootRect.bottom
      )
        return false;
      const labelRect = label.getBoundingClientRect();
      const current = [
        root.scrollTop,
        root.scrollHeight,
        labelRect.x,
        labelRect.y,
        labelRect.width,
        labelRect.height,
      ];
      const state = window.__snapshotOrderOpenStable;
      state.frames =
        state.previous &&
        current.every(
          (value, index) => Math.abs(value - state.previous[index]) < 0.1,
        )
          ? state.frames + 1
          : 0;
      state.previous = current;
      return state.frames >= 2;
    },
    undefined,
    { polling: "raf", timeout: 10000 },
  );
  await page.waitForFunction(
    () => window.__snapshotOrderLabelFrames?.stableFrames >= 1,
    undefined,
    { timeout: 5000 },
  );
  const before = await label.boundingBox();
  await page.evaluate(() => {
    window.__snapshotOrderLabelFrames.phase = "loading";
  });
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
  const geometryFrames = await page.evaluate(() => {
    const record = window.__snapshotOrderLabelFrames;
    record.phase = "done";
    return record.frames;
  });
  console.log(
    `SNAPSHOT_ORDER_LABEL_GEOMETRY ${JSON.stringify(
      geometryFrames.map(
        ({
          phase,
          label,
          fields,
          actions,
          form,
          card,
          transcriptScrollTop,
          priorSiblings,
        }) => ({
          phase,
          label,
          fields,
          actions,
          form,
          card,
          transcriptScrollTop,
          priorSiblings,
        }),
      ),
    )}`,
  );

  for (const key of ["x", "y", "width", "height"])
    assert.ok(
      Math.abs(before[key] - during[key]) <= 1,
      `Loading label ${key} stays stable; before=${JSON.stringify(before)}, during=${JSON.stringify(during)}, frames=${JSON.stringify(geometryFrames)}`,
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
    await workerButton
      .getByText(`Worker revision ${revision}`, { exact: true })
      .waitFor();
    const committedFrame = await waitFor(
      () =>
        page.evaluate(
          ({ workerId, expectedName }) =>
            window.__snapshotOrderResourceFrames.find(({ frame }) =>
              frame.resourceVersions?.some((version) =>
                version.entityChanges?.documents?.some((document) => {
                  if (document.id !== `entity:agent:${workerId}`) return false;
                  return (
                    JSON.parse(document.payload).value.name === expectedName
                  );
                }),
              ),
            ),
          { workerId: worker.id, expectedName: `Worker revision ${revision}` },
        ),
      "A real resource frame pushes the committed worker revision",
    );
    const workerRows = committedFrame.frame.resourceVersions.flatMap(
      (version) =>
        (version.entityChanges?.documents ?? []).flatMap((document) => {
          if (document.id !== `entity:agent:${worker.id}`) return [];
          const value = JSON.parse(document.payload).value;
          return [{ seq: document.seq, name: value.name }];
        }),
    );
    const committedWorkerSequence = workerRows.find(
      (row) => row.name === `Worker revision ${revision}`,
    )?.seq;
    const committedMutationSequence = mutationSequences.get(revision);
    assert.ok(
      Number.isSafeInteger(committedWorkerSequence) &&
        committedWorkerSequence === committedMutationSequence &&
        committedWorkerSequence > seededWorker.seq,
      "The pushed worker entity matches the committed mutation sequence and advances the seeded entity",
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
      await page
        .getByText("Obsolete snapshot failure", { exact: true })
        .count(),
      0,
      "An obsolete session failure does not surface its stale error",
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
  await writeFile(
    join(evidence, "requests.json"),
    JSON.stringify({ requests, button: { before, during } }, null, 2),
  );
  console.log(
    `PASS: delayed credential success and error cannot revert newer replicated data; current errors remain visible; loading button label stays fixed. ${evidence}`,
  );
}

test("snapshot-order-ui", async ({ page }) => runSnapshotOrderUi(page));

test("snapshot-order-ui request anchor @client", async ({ page }) =>
  runSnapshotOrderUi(page, { checkRequestAnchor: true }));
