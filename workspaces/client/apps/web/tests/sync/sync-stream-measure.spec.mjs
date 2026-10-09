import {
  readTestState,
  test,
  browserExecutablePath,
  spawnFixture as spawn,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import http from "node:http";
import { setTimeout as delay } from "node:timers/promises";
test("Sync stream measure @performance", async () => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Private fake-runtime measurement: fixture write commit -> persisted renderer row.

  const root = testRepo;
  const fixturePath =
    process.env.SYNC_FIXTURE_PATH ||
    join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py");
  const requireWeb = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { chromium } = requireWeb("playwright-core");
  const typescript = requireWeb("typescript");
  const { createServer } = await import(
    new URL("../../node_modules/vite/dist/node/index.js", import.meta.url)
  );
  const directory = await mkdtemp(join(tmpdir(), "sync-stream-measure-"));
  const fixture = spawn("python3", ["-B", fixturePath, directory], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(directory, "board") },
  });
  let fixtureError = "";
  fixture.stderr.on("data", (chunk) => (fixtureError += chunk));
  let pendingOutput = "";
  const replies = new Map();
  let fixturePort;
  fixture.stdout.on("data", (chunk) => {
    pendingOutput += String(chunk);
    for (;;) {
      const newline = pendingOutput.indexOf("\n");
      if (newline < 0) break;
      const line = pendingOutput.slice(0, newline).trim();
      pendingOutput = pendingOutput.slice(newline + 1);
      if (!fixturePort) fixturePort = Number(line);
      else {
        try {
          const message = JSON.parse(line);
          replies.get(message.id)?.(message);
        } catch {}
      }
    }
  });
  const waitFor = async (test, label, timeout = 15000) => {
    const end = Date.now() + timeout;
    while (Date.now() < end) {
      if (test()) return;
      await delay(30);
    }
    throw new Error(`Timed out waiting for ${label}. ${fixtureError}`);
  };
  const sendWrite = (agent, status, id) =>
    new Promise((resolve) => {
      replies.set(id, resolve);
      fixture.stdin.write(
        JSON.stringify({
          method: "fixture/sync-write",
          id,
          params: { agent, status },
        }) + "\n",
      );
    });
  let server, browser;
  try {
    await waitFor(() => fixturePort, "private fixture startup");
    const fixtureOrigin = `http://127.0.0.1:${fixturePort}`;
    const snapshot = await readTestState(fixtureOrigin);
    const lead = snapshot.threads.find((row) => row.name === "Release lead");
    assert.ok(lead?.id);
    const oldClient =
      process.env.OLD_RENDERER_MODE === "1"
        ? execFileSync(
            "git",
            [
              "show",
              "7a33c1a3dce7bb984271310cdb741e07788927b:web/src/sync/client.ts",
            ],
            { cwd: root, encoding: "utf8" },
          )
        : undefined;
    const oldClientJs = oldClient
      ? typescript.transpileModule(oldClient, {
          compilerOptions: {
            module: typescript.ModuleKind.ESNext,
            target: typescript.ScriptTarget.ES2022,
          },
        }).outputText
      : undefined;
    server = await createServer({
      configFile: false,
      root: join(root, "web"),
      server: { host: "127.0.0.1", port: 0 },
      plugins: oldClient
        ? [
            {
              name: "pure-base-sync-client",
              transform(_code, id) {
                if (id.split("?")[0].endsWith("/src/sync/client.ts"))
                  return oldClientJs;
              },
            },
          ]
        : [],
    });
    server.middlewares.use((request, response, next) => {
      if (!request.url?.startsWith("/api/")) return next();
      const headers = { ...request.headers, host: `127.0.0.1:${fixturePort}` };
      delete headers.origin;
      delete headers["sec-fetch-site"];
      const proxy = http.request(
        {
          hostname: "127.0.0.1",
          port: fixturePort,
          method: request.method,
          path: request.url,
          headers,
        },
        (upstream) => {
          response.writeHead(upstream.statusCode || 502, upstream.headers);
          upstream.pipe(response);
        },
      );
      proxy.on("error", (error) => {
        response.writeHead(502);
        response.end(String(error));
      });
      request.pipe(proxy);
    });
    const apiProxyLayer = server.middlewares.stack.pop();
    server.middlewares.stack.unshift(apiProxyLayer);
    server.middlewares.use("/sync-measure", (_request, response) => {
      response.setHeader("Content-Type", "text/html");
      response.end("<!doctype html><title>Sync renderer measurement</title>");
    });
    const checkPageLayer = server.middlewares.stack.pop();
    server.middlewares.stack.unshift(checkPageLayer);
    await server.listen();
    browser = await chromium.launch({
      headless: true,
      executablePath: browserExecutablePath,
    });
    const page = await browser.newPage();
    const apiRequests = [];
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) apiRequests.push(url.pathname);
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/sync-measure`,
    );
    await page.evaluate(
      async ({ leadId, beforeMode }) => {
        const client = await import("/src/sync/client.ts");
        const { db } = await client.syncDatabase();
        const response = await fetch(
          "/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=500&reset=1",
        );
        const baseline = await response.json();
        const row = baseline.documents.find(
          (document) => document.id === `entity:agent:${leadId}`,
        );
        if (!row) throw new Error("initial entity pull omitted lead");
        await client.persistProjection(db.projections, row);
        await client.persistProjection(db.projections, {
          id: "state:entities:checkpoint",
          payload: "{}",
          seq: baseline.checkpoint.seq,
        });
        window.syncClient = client;
        window.syncDb = db;
        window.syncCursor = baseline.checkpoint.seq;
        window.__beforeMode = beforeMode;
        window.renderedStatus = JSON.parse(row.payload).value.status;
        db.projections
          .findOne(`entity:agent:${leadId}`)
          .$.subscribe((document) => {
            if (document)
              window.renderedStatus = JSON.parse(document.payload).value.status;
          });
        window.stopSync = client.watchSyncInvalidations(async () => {
          if (window.__beforeMode) {
            const next = await (
              await fetch(
                `/api/sync/pull?scope=state%3Aentities%3Av1&after=${window.syncCursor}&limit=500`,
              )
            ).json();
            for (const document of next.documents)
              await client.persistProjection(db.projections, document);
            window.syncCursor = next.checkpoint.seq;
            await client.persistProjection(db.projections, {
              id: "state:entities:checkpoint",
              payload: "{}",
              seq: window.syncCursor,
            });
          }
          window.lastAppliedAt = Date.now();
        }, "entities");
      },
      { leadId: lead.id, beforeMode: process.env.OLD_RENDERER_MODE === "1" },
    );
    // The pure-base callback re-syncs by pull after its invalidation event.
    await waitFor(
      () => apiRequests.some((path) => path === "/api/sync/stream"),
      "change stream open",
    );
    await delay(1000);
    apiRequests.length = 0;
    const idleStart = Date.now();
    await delay(10000);
    const idleSeconds = (Date.now() - idleStart) / 1000;
    const idleCounts = Object.fromEntries(
      [...new Set(apiRequests)].map((path) => [
        path,
        apiRequests.filter((item) => item === path).length,
      ]),
    );
    const idlePerMinute = (apiRequests.length * 60) / idleSeconds;
    apiRequests.length = 0;
    const latenciesMs = [];
    const loadStart = Date.now();
    for (let index = 0; index < 5; index++) {
      const status = index % 2 ? "waiting" : "completed";
      const writeId = `measure-${Date.now()}-${index}`;
      await page.evaluate(() => {
        window.lastAppliedAt = 0;
      });
      const result = await sendWrite(lead.id, status, writeId);
      await page.waitForFunction(
        (expected) =>
          window.renderedStatus === expected && window.lastAppliedAt > 0,
        status,
        { timeout: 15000 },
      );
      const appliedAt = await page.evaluate(() => window.lastAppliedAt);
      latenciesMs.push(Math.round(appliedAt - result.committedAt * 1000));
      if (index < 4) await delay(500);
    }
    const loadSeconds = (Date.now() - loadStart) / 1000;
    const loadCounts = Object.fromEntries(
      [...new Set(apiRequests)].map((path) => [
        path,
        apiRequests.filter((item) => item === path).length,
      ]),
    );
    const loadPerMinute = (apiRequests.length * 60) / loadSeconds;
    assert.equal(latenciesMs.length, 5);
    console.log(
      JSON.stringify(
        {
          server: "private fixture with fake runtime/model",
          renderer: process.env.OLD_RENDERER_MODE
            ? "pure-base renderer"
            : "push renderer",
          writeToRendererMs: {
            samples: latenciesMs,
            median: [...latenciesMs].sort((a, b) => a - b)[2],
            max: Math.max(...latenciesMs),
          },
          idle: {
            durationSeconds: Number(idleSeconds.toFixed(1)),
            requestCounts: idleCounts,
            requestsPerMinute: Number(idlePerMinute.toFixed(1)),
          },
          load: {
            writes: 5,
            durationSeconds: Number(loadSeconds.toFixed(1)),
            requestCounts: loadCounts,
            requestsPerMinute: Number(loadPerMinute.toFixed(1)),
          },
        },
        null,
        2,
      ),
    );
  } finally {
    await browser?.close();
    await server?.close();
    fixture.stdin.end();
    await rm(directory, { recursive: true, force: true });
  }
});
