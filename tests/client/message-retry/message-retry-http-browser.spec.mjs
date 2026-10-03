// Lose a real committed HTTP response, reload, and retry the same stored intention.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, spawnFixture as spawn } from "../playwright.mjs";

test("message retry http browser", async ({ page: runnerPage }) => {
  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const state = await mkdtemp(join(tmpdir(), "studio-retry-http-"));
  const fixture = spawn(
    "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let server,
    release = () => {},
    log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const target = `http://127.0.0.1:${port}`;
    const snapshot = await (await fetch(target + "/api/state")).json();
    const lead = snapshot.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    server = await createServer({
      configFile: false,
      root: join(root, "web"),
      server: {
        host: "127.0.0.1",
        port: 0,
        proxy: {
          "/api": {
            target,
            changeOrigin: true,
            configure(proxy) {
              proxy.on("proxyReq", (request) =>
                request.setHeader("Origin", target),
              );
            },
          },
        },
      },
    });
    await server.listen();
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    const page = runnerPage;
    page.setDefaultTimeout(20000);
    const errors = [],
      attempts = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>HTTP retry</title>",
      }),
    );
    const gate = new Promise((resolve) => {
      release = resolve;
    });
    let committed = false;
    await page.route("**/api/messages", async (route) => {
      attempts.push(route.request().postDataJSON());
      const first = attempts.length === 1;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      if (first) {
        committed = true;
        await gate;
        try {
          await route.abort("failed");
        } catch {}
      } else await route.fulfill({ response });
    });
    const mount = async () => {
      await page.goto(origin + "/check");
      await page.evaluate(async () => {
        const send = await import("/src/sync/send.ts");
        window.send = send.durableSend;
        const r = await import("/node_modules/.vite/deps/react.js"),
          d = await import("/node_modules/.vite/deps/react-dom_client.js");
        const React = r.default || r,
          { createRoot } = d.default || d;
        const node = document.createElement("div");
        document.body.appendChild(node);
        createRoot(node).render(
          React.createElement(function Harness() {
            window.outbox = send.useOutbox();
            return null;
          }),
        );
      });
      await page.waitForFunction(() => !!window.outbox);
    };
    await mount();
    const id = crypto.randomUUID();
    const body = {
      id,
      room: lead.id,
      text: "One user message after a lost response",
      assets: [],
      delivery: "queue",
    };
    const start = Date.now();
    const result = await page.evaluate((body) => window.send(body), body);
    assert.equal(
      committed,
      true,
      "The server commits before the response is lost",
    );
    assert.equal(result.queued, true, "A missing response remains pending");
    assert.ok(
      Date.now() - start < 20000,
      "A lost response must not leave Sending active indefinitely",
    );
    release();
    await mount();
    await page.waitForFunction(
      (id) =>
        window.outbox.entries.some(
          (entry) => entry.id === id && entry.status === "accepted",
        ),
      id,
    );
    assert.ok(attempts.length >= 2);
    assert.ok(
      attempts.every(
        (attempt) => JSON.stringify(attempt) === JSON.stringify(body),
      ),
      "Every attempt keeps its original identity and content",
    );
    const counts = JSON.parse(
      execFileSync(
        "python3",
        [
          "-c",
          'import json,sqlite3,sys; db=sqlite3.connect(sys.argv[1]); print(json.dumps([db.execute("SELECT COUNT(*) FROM runtime_events WHERE id=?",(sys.argv[2],)).fetchone()[0],db.execute("SELECT COUNT(*) FROM messages WHERE id=?",(sys.argv[2],)).fetchone()[0]]))',
          join(state, "canvas.sqlite3"),
          id,
        ],
        { encoding: "utf8" },
      ),
    );
    assert.deepEqual(
      counts,
      [1, 1],
      "The runtime and message history each contain one stored message",
    );
    expect(errors).toEqual([]);
    console.log(
      "PASS: real committed response lost, bounded wait, reload, exact retry, one runtime message and one history row",
    );
  } finally {
    release();
    if (server) await server.close();
    fixture.stdin.end();
  }
});
