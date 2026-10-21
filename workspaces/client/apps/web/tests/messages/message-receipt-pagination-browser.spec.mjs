import { FIXTURE_OPTIMIZE_DEPS } from "../playwright.mjs";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  readTestState,
  test,
  spawnFixture as spawn,
  rustUiFixture,
  rustFixtureStateDirectory,
} from "../playwright.mjs";
// Built App and a private runtime fixture. No model calls or user state.
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { randomUUID, createHash } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

async function runMessageReceiptScenario(
  { browser: testBrowser, context: runnerContext },
  observedFirst,
) {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const { createFixtureViteServer: createServer } =
    await import("../playwright.mjs");
  const dist =
    process.env.RECEIPTS_DIST || join(repo, "workspaces/client/apps/web/dist");
  const index = await readFile(join(dist, "index.html"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-message-receipts-"));
  const id = randomUUID();
  const uncertainId = randomUUID();
  const missingId = randomUUID();
  const text = "Accepted input before the latest transcript page";
  const uncertainText = "This old delivery remains unconfirmed";
  const missingText = "An absent receipt does not confirm delivery";
  const latestPageText = "Latest transcript fixture marker";
  const fixture = spawn(
    process.execPath,
    [rustUiFixture, temporary, "--workers", "1"],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
      },
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log = (log + chunk).slice(-8000);
  });
  let server;
  const posts = [];
  const receiptRequests = [];
  const errors = [];
  let latest;
  let receiptGateReleased = false;
  let releaseReceiptGate;
  let receiptRequestPaused = false;
  const receiptGate = new Promise((resolve) => {
    releaseReceiptGate = resolve;
  });

  const changeFixture = async (agent, phase, target, token) => {
    const value = { id, uncertainId, text, uncertainText, observedFirst };
    const db = new DatabaseSync(
      join(rustFixtureStateDirectory(temporary), "canvas.sqlite3"),
    );
    db.exec("PRAGMA busy_timeout=5000");
    // The store's entity triggers call functions the server registers. This
    // connection supplies stand-ins; the server rebuilds the entities it serves.
    db.function(
      "sync_entity_payload",
      { deterministic: true, varargs: true },
      (_c, _id, json) => String(json),
    );
    db.function("sync_entity_hash", { deterministic: true }, (payload) =>
      createHash("sha256").update(String(payload)).digest("hex"),
    );
    const insertItem = db.prepare(
      "INSERT OR REPLACE INTO runtime_items(id,agent,record,created) VALUES (?,?,?,?)",
    );
    const userItem = (key, itemText, at, status, pending, materialized) => ({
      id: agent + ":" + key,
      role: "user",
      title: "You",
      clientMessageId: key,
      text: itemText,
      at,
      deliveryStatus: status,
      requestedDelivery: "after_tool",
      pending,
      materialized,
    });
    const putItem = (item) =>
      insertItem.run(item.id, agent, JSON.stringify(item), item.at);
    try {
      db.exec("BEGIN IMMEDIATE");
      if (phase === "initial") {
        db.prepare(
          "UPDATE runtime_events SET status='delivered' WHERE agent=? AND kind='user'",
        ).run(agent);
        const now = Date.now() / 1000;
        for (const [key, status, itemText] of [
          [value.id, "pending", value.text],
          [value.uncertainId, "uncertain", value.uncertainText],
        ]) {
          db.prepare(
            "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,'user',?,?,?,0)",
          ).run(key, agent, itemText, status, now - 1);
          db.prepare(
            "INSERT INTO runtime_event_meta(id,record) VALUES (?,?)",
          ).run(
            key,
            JSON.stringify({
              requestedDelivery: "after_tool",
              transcriptItemId: agent + ":" + key,
            }),
          );
          if (!value.observedFirst || key === value.uncertainId)
            putItem(
              userItem(
                key,
                itemText,
                now - 1,
                status,
                status === "pending",
                false,
              ),
            );
        }
      } else {
        const status = phase === "observed" ? "dispatching" : "delivered";
        const markItem = db.prepare(
          "UPDATE runtime_items SET record=json_set(record,'$.materialized',json('true'),'$.pending',json('false'),'$.deliveryStatus',?) WHERE id=? AND agent=?",
        );
        if (value.observedFirst && ["observed", "delivered"].includes(phase)) {
          // Match runtime.start input projection, then native userMessage delivery.
          db.prepare(
            "UPDATE runtime_events SET status=? WHERE id=? AND agent=?",
          ).run(status, value.id, agent);
          putItem(
            userItem(
              value.id,
              value.text,
              Date.now() / 1000,
              status,
              false,
              true,
            ),
          );
        }
        if (value.observedFirst)
          markItem.run(status, agent + ":" + value.id, agent);
        else {
          db.prepare(
            "UPDATE runtime_events SET status='delivered' WHERE id=? AND agent=?",
          ).run(value.id, agent);
          markItem.run("delivered", agent + ":" + value.id, agent);
        }
        if (
          phase === "paginate" ||
          (phase === "delivered" && !value.observedFirst)
        ) {
          const now = Date.now() / 1000;
          // Both aggregate event and transcript windows now omit the original.
          // The exact durable receipt remains available by its primary key.
          for (let number = 0; number < 225; number++) {
            const last = number === 224;
            putItem({
              id: agent + ":later-" + number,
              role: last ? "assistant" : "tool",
              title: last ? "Assistant" : "Tool",
              text: last
                ? "Latest transcript fixture marker"
                : "Later tool result " + number,
              at: now + number / 100,
              toolStatus: "completed",
            });
            db.prepare(
              "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,'agent_message',?,'delivered',?,0)",
            ).run(
              "later-event-" + number,
              agent,
              "Later fixture event",
              now + number / 100,
            );
          }
        }
      }
      db.exec("COMMIT");
    } finally {
      db.close();
    }
    if (
      phase === "delivered" ||
      ["observed", "paginate", "receipt-refresh"].includes(phase)
    ) {
      const response = await fetch(target + "/api/sync/notify", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Origin: target,
          "X-Canvas-Token": token,
        },
        body: JSON.stringify({
          requestId: randomUUID(),
          resources:
            phase === "delivered" && !observedFirst
              ? [
                  { kind: "queue", agentId: agent },
                  { kind: "receipts", agentId: agent },
                  { kind: "transcript", agentId: agent },
                ]
              : phase === "observed" || phase === "paginate"
                ? [{ kind: "transcript", agentId: agent }]
                : [{ kind: "receipts", agentId: agent }],
        }),
      });
      assert.equal(response.status, 200, await response.text());
    }
  };

  const until = async (check, label, timeout = 10000) => {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      if (await check()) return;
      await new Promise((resolve) => setTimeout(resolve, 30));
    }
    throw new Error(label);
  };

  try {
    const port = await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("Private fixture startup timed out: " + log)),
        // FastAPI imports alone took 21.7 seconds under live machine load.
        // Keep fixture setup separate from the delivery assertions below.
        60000,
      );
      fixture.stdout.once("data", (chunk) => {
        clearTimeout(timer);
        const value = Number(String(chunk).trim());
        if (!Number.isInteger(value))
          reject(new Error("The private fixture did not return its HTTP port"));
        else resolve(value);
      });
      fixture.once("exit", () => {
        clearTimeout(timer);
        reject(new Error("Private fixture exited: " + log));
      });
    });
    const target = `http://127.0.0.1:${port}`;
    const get = async (path) => {
      const response = await fetch(target + path, {
        signal: AbortSignal.timeout(5000),
      });
      assert.equal(
        response.status,
        200,
        path + ": " + (await response.clone().text()).slice(0, 300),
      );
      return response.json();
    };
    const state = await readTestState(target);
    // Wait for the fixture's canonical schema before the outbox flow.
    await until(
      async () => {
        const response = await fetch(target + "/api/sync/identity", {
          headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
          signal: AbortSignal.timeout(15000),
        });
        if (response.status === 503) {
          assert.match(
            await response.text(),
            /Studio API schema identity is unavailable/,
          );
          return false;
        }
        assert.equal(response.status, 200, await response.text());
        assert.equal(
          response.headers.get(API_SCHEMA_HASH_HEADER),
          readApiSchemaHash(),
        );
        return true;
      },
      "The private fixture API schema did not become ready: " + log,
      60000,
    );
    const agent = state.threads.find((agent) => agent.name === "Release lead");
    assert.ok(agent?.id);
    await changeFixture(agent.id, "initial", target, state.token);

    server = await createServer({
      configFile: false,
      root: join(repo, "workspaces/client/apps/web"),
      cacheDir: join(temporary, "vite"),
      optimizeDeps: {
        noDiscovery: true,
        include: [...FIXTURE_OPTIMIZE_DEPS.include, "rxdb", "rxjs"],
      },
      server: {
        host: "127.0.0.1",
        port: 0,
        hmr: false,
        proxy: {
          "/api": {
            target,
            changeOrigin: true,
            configure(proxy) {
              proxy.on("proxyReq", (request) =>
                request.setHeader("origin", target),
              );
            },
          },
        },
      },
    });
    await server.listen();
    const origin = server.resolvedUrls.local[0];
    const context = await testBrowser.newContext({
      viewport: { width: 1440, height: 960 },
      serviceWorkers: "block",
    });
    await context.route(origin, (route) =>
      route.fulfill({ contentType: "text/html", body: index }),
    );
    await context.route(origin + "assets/**", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      await route.fulfill({
        contentType: pathname.endsWith(".css") ? "text/css" : "text/javascript",
        body: await readFile(join(dist, pathname)),
      });
    });
    await context.route(origin + "check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Receipt seed</title>",
      }),
    );
    await context.addInitScript(
      ({ stateDir, agent }) => {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(agent),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(agent));
      },
      { stateDir: state.stateDir, agent: agent.id },
    );

    const storage = await context.newPage();
    await storage.goto(origin + "check");
    await storage.evaluate(
      async ({
        room,
        id,
        uncertainId,
        missingId,
        text,
        uncertainText,
        missingText,
      }) => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        window.receiptFixtureDb = db;
        for (const [key, message, status, receiptStatus] of [
          [id, text, "accepted", "queued"],
          [uncertainId, uncertainText, "uncertain", "uncertain"],
          [missingId, missingText, "accepted", "queued"],
        ]) {
          await db.outbox.insert({
            id: key,
            seq: 0,
            payload: JSON.stringify({
              body: {
                id: key,
                room,
                text: message,
                delivery: "after_tool",
                assets: [],
              },
              status,
              attempted: true,
              created: Date.now() - 1000,
              displayPending: true,
              receipt: { id: key, status: receiptStatus },
            }),
          });
        }
        window.readReceiptFixture = async (key) => {
          const doc = await db.outbox.findOne(key).exec();
          return doc && JSON.parse(doc.getLatest().payload);
        };
      },
      {
        room: agent.id,
        id,
        uncertainId,
        missingId,
        text,
        uncertainText,
        missingText,
      },
    );

    const page = await context.newPage();
    page.setDefaultTimeout(10000);
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (
        request.method() === "POST" &&
        ["/api/messages", "/api/queue"].includes(url.pathname)
      )
        posts.push(url.pathname);
      if (url.pathname === "/api/messages/receipts")
        receiptRequests.push({
          agent: url.searchParams.get("agent"),
          ids: JSON.parse(url.searchParams.get("ids")),
        });
    });
    // Hold the browser request so each scenario controls when the response is
    // fetched relative to transcript observation and durable delivery.
    if (observedFirst)
      await page.route("**/api/messages/receipts**", async (route) => {
        const url = new URL(route.request().url());
        const ids = JSON.parse(url.searchParams.get("ids") || "[]");
        if (!receiptGateReleased && ids.includes(id)) {
          receiptRequestPaused = true;
          await receiptGate;
        }
        await route.continue();
      });
    await page.goto(origin);
    await page.locator("#message").waitFor();
    const bubble = (message) =>
      page.locator("#messages .message.user").filter({ hasText: message });
    const readAppReceipt = (key) =>
      page.evaluate(async (messageId) => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        const doc = await db.outbox.findOne(messageId).exec();
        return doc && JSON.parse(doc.getLatest().payload);
      }, key);
    await bubble(text)
      .getByRole("status")
      .filter({ hasText: "Waiting for agent" })
      .waitFor();
    await bubble(uncertainText)
      .getByText("Delivery unconfirmed", { exact: true })
      .waitFor();
    // The delivery status and the message actions must not overlap.
    await bubble(uncertainText).hover();
    const boxes = await bubble(uncertainText).evaluate((node) => {
      const status = node.querySelector(".message-delivery-heading");
      const actions = node.querySelector(".message-bottom");
      return [status, actions].map((element) => {
        const rect = element?.getBoundingClientRect();
        return rect
          ? {
              left: rect.left,
              right: rect.right,
              top: rect.top,
              bottom: rect.bottom,
            }
          : null;
      });
    });
    const [statusBox, actionsBox] = boxes;
    if (statusBox && actionsBox)
      assert.ok(
        statusBox.bottom <= actionsBox.top ||
          actionsBox.bottom <= statusBox.top ||
          statusBox.right <= actionsBox.left ||
          actionsBox.right <= statusBox.left,
        `Delivery status overlaps message actions: ${JSON.stringify(boxes)}`,
      );
    if (process.env.DELIVERY_SCREENSHOT)
      await bubble(uncertainText).screenshot({
        path: process.env.DELIVERY_SCREENSHOT,
      });
    assert.equal(
      posts.length,
      0,
      "Accepted intentions must never be sent again",
    );

    if (observedFirst) {
      await until(
        () => receiptRequestPaused,
        "The app tab did not start a receipt request for the accepted message",
      );
      await changeFixture(agent.id, "observed", target, state.token);
      await until(
        async () => (await readAppReceipt(id))?.displayPending === false,
        "The transcript did not retire the local copy before the delivered receipt",
      );
      const hiddenAccepted = await readAppReceipt(id);
      assert.equal(hiddenAccepted.status, "accepted");
      assert.equal(hiddenAccepted.receipt?.status, "queued");
      assert.equal(hiddenAccepted.displayPending, false);
      await changeFixture(agent.id, "delivered", target, state.token);
      const deliveredReceiptResponse = page.waitForResponse((response) => {
        const url = new URL(response.url());
        return (
          url.pathname === "/api/messages/receipts" &&
          url.searchParams.get("agent") === agent.id &&
          JSON.parse(url.searchParams.get("ids") || "[]").includes(id)
        );
      });
      receiptGateReleased = true;
      releaseReceiptGate();
      const response = await deliveredReceiptResponse;
      const deliveredResponse = {
        status: response.status(),
        schema: response.headers()[API_SCHEMA_HASH_HEADER.toLowerCase()],
        agent: new URL(response.url()).searchParams.get("agent"),
        body: await response.json(),
      };
      assert.equal(deliveredResponse.status, 200);
      assert.equal(deliveredResponse.schema, readApiSchemaHash());
      assert.equal(deliveredResponse.agent, agent.id);
      assert.ok(
        deliveredResponse.body.items.some(
          (item) =>
            item.id === id &&
            item.status === "delivered" &&
            item.materialized === true,
        ),
      );

      try {
        await until(
          async () => {
            const value = await readAppReceipt(id);
            return (
              value?.status === "accepted" &&
              value.receipt?.status === "delivered" &&
              value.receipt?.materialized === true &&
              value.displayPending === false
            );
          },
          "The accepted outbox entry did not reconcile the delivered receipt",
          5000,
        );
      } catch (error) {
        const initialCount = receiptRequests.filter(
          (request) => request.agent === agent.id && request.ids.includes(id),
        ).length;
        for (let notification = 1; notification <= 3; notification += 1) {
          const before = receiptRequests.filter(
            (request) => request.agent === agent.id,
          ).length;
          await changeFixture(agent.id, "receipt-refresh", target, state.token);
          await until(
            () =>
              receiptRequests.filter((request) => request.agent === agent.id)
                .length > before,
            `Receipt notification ${notification} did not trigger a receipt request`,
          );
        }
        const requestsContainingIdAfterNotifications =
          receiptRequests.filter(
            (request) => request.agent === agent.id && request.ids.includes(id),
          ).length - initialCount;
        const visibleBeforeReload = {
          bubbleCount: await bubble(text).count(),
          statuses: await bubble(text).getByRole("status").allTextContents(),
        };
        await page.reload();
        await page.locator("#message").waitFor();
        const storedAfterReload = await readAppReceipt(id);
        const visibleAfterReload = {
          bubbleCount: await bubble(text).count(),
          statuses: await bubble(text).getByRole("status").allTextContents(),
        };
        throw new Error(
          `${error.message}: ${JSON.stringify({ storedInApp: await readAppReceipt(id), requestsContainingIdAfterThreeNotifications: requestsContainingIdAfterNotifications, storedAfterReload, visibleBeforeReload, visibleAfterReload, posts, errors })}`,
        );
      }
      return;
    }

    await changeFixture(agent.id, "delivered", target, state.token);
    latest = await get(`/api/transcript?id=${agent.id}`);
    assert.equal(latest.items.length, 120);
    assert.ok(
      !latest.items.some(
        (item) =>
          item.clientMessageId === id || item.id === `${agent.id}:${id}`,
      ),
    );
    assert.deepEqual((await get(`/api/queue?agent=${agent.id}`)).items, []);
    assert.ok(
      !(await readTestState(target)).runtime.events.some(
        (event) => event.id === id,
      ),
    );
    try {
      await until(async () => {
        const value = await storage.evaluate(
          (key) => window.readReceiptFixture(key),
          id,
        );
        return value?.receipt?.status === "delivered";
      }, "The exact delivered receipt did not reconcile an accepted message outside the latest 120 items");
    } catch (error) {
      const status = await bubble(text).getByRole("status").allTextContents();
      throw new Error(
        `${error.message}: ${JSON.stringify({ status, receiptRequestCount: receiptRequests.length, posts, appReceipt: await readAppReceipt(id), durable: await storage.evaluate(async (key) => (await window.receiptFixtureDb.outbox.storageInstance.findDocumentsById([key], false)).map((doc) => JSON.parse(doc.payload)), id) })}`,
      );
    }

    await bubble(text)
      .getByRole("status")
      .filter({ hasText: "Waiting for agent" })
      .waitFor({ state: "detached" });
    assert.equal(
      await bubble(text)
        .getByRole("status")
        .filter({ hasText: "Waiting for agent" })
        .count(),
      0,
    );
    assert.equal(
      posts.length,
      0,
      "Receipt reconciliation must use GET and keep the original message identity",
    );
    const delivered = await storage.evaluate(
      (key) => window.readReceiptFixture(key),
      id,
    );
    assert.equal(delivered.body.id, id);
    assert.equal(delivered.body.text, text);
    assert.equal(delivered.status, "accepted");
    assert.equal(delivered.receipt.status, "delivered");
    assert.equal(delivered.receipt.materialized, true);
    assert.equal(delivered.displayPending, false);
    const uncertain = await storage.evaluate(
      (key) => window.readReceiptFixture(key),
      uncertainId,
    );
    assert.equal(uncertain.status, "uncertain");
    assert.equal(uncertain.receipt.status, "uncertain");
    await bubble(uncertainText)
      .getByText("Delivery unconfirmed", { exact: true })
      .waitFor();
    const missing = await storage.evaluate(
      (key) => window.readReceiptFixture(key),
      missingId,
    );
    assert.equal(
      missing.receipt.status,
      "queued",
      "An omitted receipt must not become confirmed delivery",
    );
    await bubble(missingText)
      .getByRole("status")
      .filter({ hasText: "Waiting for agent" })
      .waitFor();

    await page.reload();
    await page.locator("#message").waitFor();
    await page.getByText(latestPageText, { exact: true }).waitFor();
    await until(
      async () => (await bubble(text).count()) === 0,
      "A delivered local copy reappeared after reload",
    );
    await page
      .getByRole("button", { name: "Earlier messages", exact: true })
      .waitFor({ state: "visible" });
    await page
      .getByRole("button", { name: "Earlier messages", exact: true })
      .click();
    await until(
      async () => (await bubble(text).count()) === 1,
      "The saved original is missing from its history page",
    );
    assert.equal(await bubble(text).count(), 1);
    assert.equal(
      posts.length,
      0,
      "History recovery must not resend or cancel input",
    );
    assert.ok(
      receiptRequests.some(
        (request) => request.agent === agent.id && request.ids.includes(id),
      ),
    );
    assert.ok(
      receiptRequests.every(
        (request) => request.agent === agent.id && request.ids.length <= 100,
      ),
    );
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        status: "passed",
        dist,
        indexSha256: createHash("sha256").update(index).digest("hex"),
        latestItems: latest.items.length,
        receiptRequests: receiptRequests.length,
        messagePosts: posts.length,
      }),
    );
  } finally {
    receiptGateReleased = true;
    releaseReceiptGate();
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server?.close();
    await rm(temporary, {
      recursive: true,
      force: true,
      maxRetries: 5,
      retryDelay: 100,
    });
  }
}

test("Message receipt pagination browser", async ({ browser, context }) => {
  test.setTimeout(45_000);
  await runMessageReceiptScenario({ browser, context }, false);
});

test("delivered receipt reconciles a hidden accepted outbox entry", async ({
  browser,
  context,
}) => {
  test.setTimeout(30_000);
  await runMessageReceiptScenario({ browser, context }, true);
});
