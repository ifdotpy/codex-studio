#!/usr/bin/env node
// Built App and a private runtime fixture. No model calls or user state.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { randomUUID, createHash } from "node:crypto";
import { createRequire } from "node:module";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const python = process.env.PYTHON_BIN || "/opt/homebrew/bin/python3.14";
const dist = process.env.RECEIPTS_DIST || join(repo, "web/dist");
const index = await readFile(join(dist, "index.html"));
const temporary = await mkdtemp(join(tmpdir(), "studio-message-receipts-"));
const id = randomUUID();
const uncertainId = randomUUID();
const missingId = randomUUID();
const text = "Accepted input before the latest transcript page";
const uncertainText = "This old delivery remains unconfirmed";
const missingText = "An absent receipt does not confirm delivery";
const fixture = spawn(
  python,
  ["-B", join(repo, "tests/simple-ui-fixture.py"), temporary],
  {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_BOARD_STATE_DIR: join(temporary, "board"),
      TOKEN_RATE_WORKER_COUNT: "1",
    },
  },
);
let log = "";
fixture.stderr.on("data", (chunk) => {
  log = (log + chunk).slice(-8000);
});
let browser, server;
const posts = [];
const receiptRequests = [];
const errors = [];

const changeFixture = (agent, phase) => {
  execFileSync(
    python,
    [
      "-B",
      "-c",
      `import json, pathlib, sys, time
sys.path.insert(0, sys.argv[1])
from codex_sqlite import connect
from codex_sync_entities import register_functions
root, agent, phase = pathlib.Path(sys.argv[2]), sys.argv[3], sys.argv[4]
value = json.loads(sys.argv[5])
db = connect(root / 'canvas.sqlite3', timeout=1, site='Receipt pagination fixture')
register_functions(db)
try:
    with db:
        if phase == 'initial':
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=? AND kind='user'", (agent,))
            now = time.time()
            for key, status, text in ((value['id'], 'pending', value['text']),
                                      (value['uncertainId'], 'uncertain', value['uncertainText'])):
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,'user',?,?,?,0)",
                           (key, agent, text, status, now - 1))
                db.execute("INSERT INTO runtime_event_meta(id,record) VALUES (?,?)",
                           (key, json.dumps({'requestedDelivery': 'after_tool', 'transcriptItemId': agent + ':' + key})))
                record = {'id': agent + ':' + key, 'role': 'user', 'title': 'You',
                          'clientMessageId': key, 'text': text, 'at': now - 1,
                          'deliveryStatus': status, 'requestedDelivery': 'after_tool',
                          'pending': status == 'pending', 'materialized': False}
                db.execute('INSERT INTO runtime_items(id,agent,record,created) VALUES (?,?,?,?)',
                           (record['id'], agent, json.dumps(record), record['at']))
        else:
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id=? AND agent=?", (value['id'], agent))
            now = time.time()
            # Both aggregate event and transcript windows now omit the original.
            # The exact durable receipt remains available by its primary key.
            for number in range(125):
                item = {'id': agent + ':later-' + str(number), 'role': 'tool', 'title': 'Tool',
                        'text': 'Later tool result ' + str(number), 'at': now + number / 100,
                        'toolStatus': 'completed'}
                db.execute('INSERT INTO runtime_items(id,agent,record,created) VALUES (?,?,?,?)',
                           (item['id'], agent, json.dumps(item), item['at']))
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,'agent_message',?,'delivered',?,0)",
                           ('later-event-' + str(number), agent, 'Later fixture event', item['at']))
        db.commit()
finally:
    db.close()
`,
      join(repo, "scripts"),
      temporary,
      agent,
      phase,
      JSON.stringify({ id, uncertainId, text, uncertainText }),
    ],
    { timeout: 5000, stdio: "pipe" },
  );
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
      15000,
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
  const state = await get("/api/state?view=chat");
  const agent = state.threads.find((agent) => agent.name === "Release lead");
  assert.ok(agent?.id);
  changeFixture(agent.id, "initial");

  server = await createServer({
    configFile: false,
    root: join(repo, "web"),
    cacheDir: join(temporary, "vite"),
    optimizeDeps: { include: ["rxdb", "rxjs"] },
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
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const context = await browser.newContext({
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
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
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
  await page.goto(origin);
  await page.locator("#message").waitFor();
  const bubble = (message) =>
    page.locator("#messages .message.user").filter({ hasText: message });
  await bubble(text)
    .getByRole("status")
    .filter({ hasText: "Waiting for agent" })
    .waitFor();
  await bubble(uncertainText)
    .getByText("Delivery unconfirmed", { exact: true })
    .waitFor();
  assert.equal(posts.length, 0, "Accepted intentions must never be sent again");

  changeFixture(agent.id, "delivered");
  const latest = await get(`/api/transcript?id=${agent.id}`);
  assert.equal(latest.items.length, 120);
  assert.ok(
    !latest.items.some(
      (item) => item.clientMessageId === id || item.id === `${agent.id}:${id}`,
    ),
  );
  assert.deepEqual((await get(`/api/queue?agent=${agent.id}`)).items, []);
  assert.ok(
    !(await get("/api/state?view=chat")).runtime.events.some(
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
      `${error.message}: ${JSON.stringify({ status, receiptRequestCount: receiptRequests.length, posts })}`,
    );
  }
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
  await browser?.close();
  await server?.close();
  fixture.kill("SIGTERM");
  if (fixture.exitCode === null)
    await new Promise((resolve) => fixture.once("exit", resolve));
  await rm(temporary, { recursive: true, force: true });
}
