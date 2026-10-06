import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  syncProtocolFixture,
  test,
} from "../playwright.mjs";

const workspaceId = "a".repeat(32);

async function createFixture(page, options = {}) {
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const require = createRequire(join(repo, "web/package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const server = await createServer({
    configFile: false,
    root: join(repo, "web"),
    server: { host: "127.0.0.1", port: 0 },
  });
  let available = options.available ?? true;
  let identityFailures = options.identityFailures ?? 0;
  let pullFailures = options.pullFailures ?? 0;
  let pullStatus = options.pullStatus;
  const streams = new Set();
  let revision = 1;
  const requests = [];
  const counts = { identity: 0, stream: 0, pull: 0 };
  const json = (res, status, value) => {
    res.writeHead(status, { "Content-Type": "application/json" });
    res.end(JSON.stringify(value));
  };
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url, "http://fixture.local");
      requests.push(`${req.method} ${url.pathname}${url.search}`);
      if (url.pathname === "/check") {
        res.end("<!doctype html><div id='root'></div>");
      } else if (url.pathname === "/api/session") {
        json(res, 200, { token: "fixture-token" });
      } else if (url.pathname === "/api/sync/protocol") {
        json(res, 200, syncProtocolFixture());
      } else if (url.pathname === "/api/sync/identity") {
        counts.identity++;
        if (!available || identityFailures > 0) {
          if (identityFailures > 0) identityFailures--;
          json(res, 503, { error: "Fixture sync unavailable" });
        } else json(res, 200, { workspaceId });
      } else if (url.pathname === "/api/sync/stream") {
        counts.stream++;
        if (!available) {
          res.writeHead(503, { "Content-Type": "text/plain" });
          res.end("Fixture stream unavailable");
        } else {
          const resources = JSON.parse(
            url.searchParams.get("resources") || "[]",
          );
          res.writeHead(200, {
            "Cache-Control": "no-cache",
            "Content-Type": "text/event-stream",
          });
          res.write(
            apiSchemaHandshakeSse(
              protocol3SseEvent("resources", {
                protocol: 3,
                workspaceId,
                epoch: "fixture-epoch",
                revision,
                reason: "initial",
                resources,
              }),
            ),
          );
          const stream = { res, resources };
          streams.add(stream);
          res.on("close", () => streams.delete(stream));
        }
      } else if (url.pathname === "/api/sync/pull") {
        counts.pull++;
        if (!available) {
          json(res, 503, { error: "Entity sync is unavailable" });
        } else if (pullFailures > 0) {
          pullFailures--;
          json(res, 503, { error: "Temporary entity sync failure" });
        } else if (pullStatus !== undefined) {
          json(res, pullStatus, { error: "Configured pull failure" });
        } else {
          json(res, 200, {
            workspaceId,
            documents: [],
            checkpoint: { seq: 0 },
            initialHigh: 0,
            maxSeq: 0,
          });
        }
      } else next();
    },
  });
  await server.listen();
  try {
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      const { useSnapshot } = await import("/src/hooks.ts");
      const ReactModule = await import("/node_modules/.vite/deps/react.js");
      const DomModule =
        await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = ReactModule.default || ReactModule;
      const { createRoot } = DomModule.default || DomModule;
      const root = createRoot(document.getElementById("root"));
      window.unmountSnapshot = () => root.unmount();
      root.render(
        React.createElement(function SnapshotHarness() {
          window.snapshot = useSnapshot();
          return React.createElement(
            "p",
            { role: window.snapshot.error ? "alert" : "status" },
            window.snapshot.error || "Connecting to Codex Studio…",
          );
        }),
      );
    });
  } catch (error) {
    await server.close();
    throw error;
  }
  return {
    counts,
    requests,
    async close() {
      await page.evaluate(() => window.unmountSnapshot?.());
      await server.close();
    },
    setPullStatus(value) {
      pullStatus = value;
    },
    setAvailable(value) {
      available = value;
      if (value) this.publish();
    },
    publish() {
      revision++;
      const frame = protocol3SseEvent("resources", {
        protocol: 3,
        workspaceId,
        epoch: "fixture-epoch",
        revision,
        reason: "change",
        resources: [{ kind: "state" }],
      });
      for (const stream of streams) {
        if (stream.resources.some((resource) => resource.kind === "state"))
          stream.res.write(frame);
      }
    },
  };
}

async function mount(page, options) {
  const fixture = await createFixture(page, options);
  await page.waitForFunction(() => Boolean(window.snapshot));
  return fixture;
}

async function waitForData(page) {
  await page.waitForFunction(
    () => Boolean(window.snapshot) && window.snapshot.data !== null,
    null,
    {
      timeout: 20_000,
    },
  );
}

test("first load reports failure and recovers without a legacy request", async ({
  page,
}) => {
  test.setTimeout(60_000);
  const fixture = await mount(page, { available: false });
  try {
    await page.waitForFunction(
      () => window.snapshot?.data === null && Boolean(window.snapshot.error),
    );
    assert.notEqual((await page.getByRole("alert").textContent())?.trim(), "");
    assert.equal(
      fixture.requests.some((request) => request.includes("/api/state")),
      false,
    );
    fixture.setAvailable(true);
    await waitForData(page);
    await page.waitForFunction(() => window.snapshot.error === "");
    assert.equal(
      fixture.requests.some((request) => request.includes("/api/state")),
      false,
    );
    assert.ok(fixture.counts.identity >= 2);
  } finally {
    await fixture.close();
  }
});

test("retries a first identity failure and then loads the entity projection", async ({
  page,
}) => {
  test.setTimeout(30_000);
  const fixture = await mount(page, { identityFailures: 1 });
  try {
    await waitForData(page);
    assert.ok(fixture.counts.identity >= 2);
  } finally {
    await fixture.close();
  }
});

test("retries the initial pull after three transient failures", async ({
  page,
}) => {
  test.setTimeout(30_000);
  const fixture = await mount(page, { pullFailures: 3 });
  try {
    await waitForData(page);
    assert.ok(fixture.counts.pull >= 4);
    await page.waitForFunction(() => window.snapshot.error === "");
  } finally {
    await fixture.close();
  }
});

test("keeps the error visible and bounds retries for a permanent 503", async ({
  page,
}) => {
  test.setTimeout(30_000);
  const fixture = await mount(page, { pullStatus: 503 });
  try {
    await page.waitForFunction(
      () => window.snapshot?.data === null && Boolean(window.snapshot.error),
    );
    await page.waitForTimeout(6_000);
    assert.equal(await page.evaluate(() => window.snapshot.data), null);
    assert.notEqual((await page.getByRole("alert").textContent())?.trim(), "");
    assert.ok(
      fixture.counts.pull <= 10,
      `pull retry count=${fixture.counts.pull}`,
    );
  } finally {
    await fixture.close();
  }
});

test("does not retry a permanent 400", async ({ page }) => {
  test.setTimeout(20_000);
  const fixture = await mount(page, { pullStatus: 400 });
  try {
    await page.waitForFunction(
      () => window.snapshot?.data === null && Boolean(window.snapshot.error),
    );
    await page.waitForTimeout(2_000);
    assert.equal(fixture.counts.pull, 1);
  } finally {
    await fixture.close();
  }
});

test("clears a later subscription error after a successful pull", async ({
  page,
}) => {
  test.setTimeout(30_000);
  const fixture = await mount(page, {});
  try {
    await waitForData(page);
    const initial = await page.evaluate(() => window.snapshot.data);
    fixture.setPullStatus(503);
    fixture.publish();
    await page.waitForFunction(() => Boolean(window.snapshot.error), null, {
      timeout: 10_000,
    });
    assert.deepEqual(await page.evaluate(() => window.snapshot.data), initial);
    fixture.setPullStatus(undefined);
    fixture.publish();
    await page.waitForFunction(() => window.snapshot.error === "", null, {
      timeout: 10_000,
    });
    assert.ok(await page.evaluate(() => window.snapshot.data));
  } finally {
    await fixture.close();
  }
});
