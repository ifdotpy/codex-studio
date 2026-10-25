import { SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS } from "../../src/generated/sharedValues.ts";
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

/** Later rows that move the accepted input outside the live window, in items. */
const LATER_TRANSCRIPT_ITEMS = 225;

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
  let releaseFreshTranscript = () => {};

  const changeFixture = async (agent, phase, target, token) => {
    const value = { id, uncertainId, text, uncertainText, observedFirst };
    const db = new DatabaseSync(
      join(rustFixtureStateDirectory(temporary), "canvas.sqlite3"),
    );
    db.exec("PRAGMA busy_timeout=5000");
    // The store's entity triggers call functions the server registers. This
    // connection preserves the event envelope owned by runtime-store/schema.rs.
    db.function(
      "sync_entity_payload",
      { deterministic: true, varargs: true },
      (collection, key, json, deleted) =>
        JSON.stringify({
          collection,
          id: key,
          value: deleted ? {} : JSON.parse(String(json)),
        }),
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
      requestedDelivery: "current_turn",
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
              requestedDelivery: "current_turn",
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
          for (let number = 0; number < LATER_TRANSCRIPT_ITEMS; number++) {
            const last = number === LATER_TRANSCRIPT_ITEMS - 1;
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
      // This external SQLite fixture is a writer: publish the exact summary
      // with its source change, rather than asking the server to rebuild it.
      for (const messageId of [value.id, value.uncertainId]) {
        const event = db
          .prepare(
            "SELECT id,agent,status,error FROM runtime_events WHERE id=?",
          )
          .get(messageId);
        if (!event) continue;
        const item = db
          .prepare("SELECT record FROM runtime_items WHERE id=? AND agent=?")
          .get(agent + ":" + messageId, agent);
        const value = { ...event, materialized: !!item };
        const payload = JSON.stringify({
          collection: "messageReceipt",
          id: messageId,
          value,
        });
        const seq = db
          .prepare(
            "SELECT max(seq)+1 AS seq FROM (SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL SELECT COALESCE(MAX(seq),0) FROM sync_versions)",
          )
          .get().seq;
        db.prepare(
          "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('messageReceipt',?,?,?,?,0) ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=0",
        ).run(
          messageId,
          seq,
          createHash("sha256").update(payload).digest("hex"),
          payload,
        );
      }
      // This external writer also owns its queue summary; a notification does
      // not ask the server to reconstruct values from an event hint.
      const epoch = JSON.parse(
        db.prepare("SELECT record FROM runtime_agents WHERE id=?").get(agent)
          .record,
      ).epoch;
      const queueItems = db
        .prepare(
          "SELECT id,kind,text,status,created,error FROM runtime_events WHERE agent=? AND epoch=? AND status IN ('pending','reserved') AND kind IN ('user','followup') ORDER BY created,rowid",
        )
        .all(agent, epoch)
        .map((event) => ({
          ...event,
          status: "pending",
          assets: [],
          delivery: "current_turn",
          requestedDelivery: "current_turn",
        }));
      const queueValue = {
        items: queueItems,
        capabilities: { reorder: true, receipts: true },
        revision: createHash("sha256")
          .update(JSON.stringify([epoch, queueItems]))
          .digest("hex"),
      };
      const queuePayload = JSON.stringify({
        collection: "queue",
        id: agent,
        value: queueValue,
      });
      const queueSeq = db
        .prepare(
          "SELECT max(seq)+1 AS seq FROM (SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL SELECT COALESCE(MAX(seq),0) FROM sync_versions)",
        )
        .get().seq;
      db.prepare(
        "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('queue',?,?,?,?,0) ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=0",
      ).run(
        agent,
        queueSeq,
        createHash("sha256").update(queuePayload).digest("hex"),
        queuePayload,
      );
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
          resources: [
            { kind: "state" },
            { kind: "transcript", agentId: agent },
          ],
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
                delivery: "current_turn",
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
    await page.goto(origin);
    await page.locator("#message").waitFor();
    const bubble = (message) =>
      page.locator("#messages .message.user").filter({ hasText: message });
    // Keep the built renderer's runtime isolated from Vite's source dependencies.
    // The seed tab reads the same durable outbox without loading a second Dexie
    // version into the app tab.
    const readOutboxReceipt = (key) =>
      storage.evaluate(
        (messageId) => window.readReceiptFixture(messageId),
        key,
      );
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

    const startupReceiptReads = receiptRequests.length;
    if (observedFirst) {
      await changeFixture(agent.id, "observed", target, state.token);
      await until(
        async () => (await readOutboxReceipt(id))?.displayPending === false,
        "The transcript did not retire the local copy before the delivered receipt",
      );
      const hiddenAccepted = await readOutboxReceipt(id);
      assert.equal(hiddenAccepted.status, "accepted");
      assert.equal(hiddenAccepted.receipt?.status, "dispatching");
      assert.equal(hiddenAccepted.displayPending, false);
      await bubble(text)
        .getByRole("status")
        .filter({ hasText: "Waiting for agent" })
        .waitFor({ state: "detached" });
      await changeFixture(agent.id, "delivered", target, state.token);
      await until(async () => {
        const value = await readOutboxReceipt(id);
        return (
          value?.status === "accepted" &&
          value.receipt?.status === "delivered" &&
          value.receipt?.materialized === true &&
          value.displayPending === false
        );
      }, "The streamed receipt did not reconcile the observed input");
      assert.equal(
        receiptRequests.length,
        startupReceiptReads,
        "streamed receipt updates add zero idle GETs",
      );
      assert.equal(
        posts.length,
        0,
        "Hidden receipt reconciliation must not resend input",
      );
      assert.deepEqual(errors, []);
      return;
    }

    const shortCachedTranscript = await storage.evaluate(async (agentId) => {
      const collection = window.receiptFixtureDb.projections;
      // Read the committed IndexedDB rows independently of the seed tab's
      // RxDB instance; the built renderer owns the active projection writer.
      const name = `rxdb-dexie-${collection.database.name}--${collection.schema.version}--projections`;
      const db = await new Promise((resolve, reject) => {
        const request = indexedDB.open(name);
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      const read = (id) =>
        new Promise((resolve, reject) => {
          const request = db
            .transaction("docs", "readonly")
            .objectStore("docs")
            .get(id);
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        });
      try {
        const meta = await read(`transcript:${agentId}`);
        const payload = JSON.parse(meta.payload);
        const items = await Promise.all(
          payload.order.map((key) => read(`transcript:item:${agentId}:${key}`)),
        );
        return [meta, ...items].map(({ id, payload, seq }) => ({
          id,
          payload,
          seq,
        }));
      } finally {
        db.close();
      }
    }, agent.id);
    assert.equal(
      JSON.parse(shortCachedTranscript[0].payload).metadata.truncated,
      false,
    );
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
        `${error.message}: ${JSON.stringify({ status, receiptRequestCount: receiptRequests.length, posts, outboxReceipt: await readOutboxReceipt(id), durable: await storage.evaluate(async (key) => (await window.receiptFixtureDb.outbox.storageInstance.findDocumentsById([key], false)).map((doc) => JSON.parse(doc.payload)), id) })}`,
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
      "Receipt reconciliation must preserve the original message identity",
    );
    assert.equal(
      receiptRequests.length,
      startupReceiptReads,
      "receipt idle GET count is zero",
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

    // Reopen with the real pre-delivery cache before allowing fresh transport.
    // Otherwise a fast pull can hide the short-cache -> truncated-page race.
    {
      await page.goto(origin + "check");
      await storage.evaluate(async (rows) => {
        for (const row of [...rows.slice(1), rows[0]])
          await window.receiptFixtureDb.projections.incrementalUpsert(row);
      }, shortCachedTranscript);
      const freshTranscriptGate = new Promise((resolve) => {
        releaseFreshTranscript = resolve;
      });
      await page.route("**/api/sync/**", async (route) => {
        const url = new URL(route.request().url());
        if (
          url.pathname === "/api/sync/stream" ||
          (url.pathname === "/api/sync/pull" &&
            url.searchParams.get("scope") === `transcript:${agent.id}`)
        )
          await freshTranscriptGate;
        await route.continue();
      });
      await page.goto(origin);
      await bubble(text).waitFor();
      assert.equal(
        await page
          .getByRole("button", { name: "Earlier messages", exact: true })
          .count(),
        0,
      );
      releaseFreshTranscript();
    }
    await page.locator("#message").waitFor();
    await page.getByText(latestPageText, { exact: true }).waitFor();
    await until(
      async () => (await bubble(text).count()) === 0,
      "A delivered local copy reappeared after reload",
    );
    await page
      .getByRole("button", { name: "Earlier messages", exact: true })
      .waitFor({ state: "visible" });
    const maximumPages = Math.ceil(
      LATER_TRANSCRIPT_ITEMS / SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS,
    );
    for (
      let index = 0;
      index < maximumPages && !(await bubble(text).count());
      index++
    ) {
      const received = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/transcript/page",
      );
      const earlier = page.getByRole("button", {
        name: "Earlier messages",
        exact: true,
      });
      await earlier.click();
      const response = await received;
      assert.equal(
        Number(new URL(response.url()).searchParams.get("limit")),
        SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS,
      );
      assert.ok(
        (await response.json()).items.length <=
          SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS,
      );
      await until(
        async () =>
          !(await earlier.count()) ||
          (await earlier.getAttribute("data-loading")) !== "true",
        "The bounded history page did not settle",
      );
    }
    await until(
      async () => (await bubble(text).count()) === 1,
      "The saved original is missing from its bounded history pages",
    );
    assert.equal(await bubble(text).count(), 1);
    assert.equal(
      posts.length,
      0,
      "History recovery must not resend or cancel input",
    );
    // Startup/resume outbox recovery is an allowed read; ordinary stream
    // updates above were measured independently.
    assert.ok(receiptRequests.every((request) => request.agent === agent.id));
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        status: "passed",
        dist,
        indexSha256: createHash("sha256").update(index).digest("hex"),
        latestItems: latest.items.length,
        receiptRecoveryRequests: receiptRequests.length,
        idleReceiptGets: 0,
        messagePosts: posts.length,
      }),
    );
  } finally {
    releaseFreshTranscript();
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
