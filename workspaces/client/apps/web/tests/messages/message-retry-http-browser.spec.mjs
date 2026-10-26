import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
  queryFixtureDatabase,
  createFixtureViteServer,
} from "../playwright.mjs";
// Lose a real committed HTTP response, reload, and retry the same stored intention.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("message retry http browser", async ({ page: runnerPage }) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-retry-http-"));
  const fixture = spawn(process.execPath, [rustUiFixture, state], {
    stdio: ["pipe", "pipe", "pipe"],
  });
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
    const snapshot = await readTestState(target);
    const lead = snapshot.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    server = await createFixtureViteServer({
      configFile: false,
      root: join(root, "workspaces/client/apps/web"),
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
    let markCommitted;
    const committed = new Promise((resolve) => {
      markCommitted = resolve;
    });
    await page.route("**/api/messages", async (route) => {
      attempts.push(route.request().postDataJSON());
      const first = attempts.length === 1;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      if (first) {
        markCommitted();
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
    await page.clock.install();
    await page.evaluate((body) => {
      window.sendResult = window.send(body);
    }, body);
    await committed;
    await page.clock.setSystemTime(
      (await page.evaluate(() => Date.now())) + 16_000,
    );
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    const result = await page.evaluate(() => window.sendResult);
    assert.equal(result.queued, true, "A missing response remains pending");
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
    const counts = [
      queryFixtureDatabase(
        state,
        "SELECT COUNT(*) AS n FROM runtime_events WHERE id=?",
        id,
      )[0].n,
      queryFixtureDatabase(
        state,
        "SELECT COUNT(*) AS n FROM messages WHERE id=?",
        id,
      )[0].n,
    ];
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
