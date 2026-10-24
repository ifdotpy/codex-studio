import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  createFixtureViteServer,
  entityPullFixture,
  handleEntitySyncFixtureRequest,
  protocol3SseEvent,
  test,
  expect,
} from "../playwright.mjs";

// Render the real App against an isolated server. Updates are complete v4 batches;
// the fixture refuses a limits read so a hidden hydration/poll cannot pass.
test("limits and Studio errors update from entities with no idle HTTP refresh", async ({
  page,
}, testInfo) => {
  test.setTimeout(90_000);
  const workspaceId = "b".repeat(32);
  const agent = {
    id: "lead",
    rootId: "lead",
    name: "Entity limits",
    source: "managed",
    status: "completed",
    accountKey: "default",
    isLead: true,
    provider: "codex",
    model: "gpt-6",
    cwd: "/fixture",
    created: Date.now() / 1000,
    inFlight: false,
    canSend: true,
  };
  const limits = (usedPercent, at) => ({
    accountKey: "default",
    at,
    data: {
      rateLimits: {
        limitId: "codex",
        primary: {
          usedPercent,
          windowDurationMins: 300,
          resetsAt: 4_000_000_000,
        },
      },
    },
  });
  const snapshot = {
    stateDir: "entity-fixture",
    chats: [],
    edges: [],
    runtime: {
      agents: [agent],
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
      nativeNotices: [],
      sidebarOrder: { groups: {}, revision: 0 },
      rateLimitsByAccount: { default: limits(11, 100) },
    },
  };
  let through = entityPullFixture(snapshot).maxSeq;
  const streams = new Set();
  const requests = [];
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.stack || error.message));
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "limits-errors-app",
        resolveId(id) {
          if (id === "/entity-fixture.tsx") return "\0entity-fixture";
        },
        load(id) {
          if (id !== "\0entity-fixture") return;
          return `import React from 'react'; import {createRoot} from 'react-dom/client';
          import {MantineProvider} from '@mantine/core'; import '@mantine/core/styles.css';
          import App from '/src/App.tsx'; import {theme} from '/src/theme.ts';
          import '/src/style.css';
          createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,{theme},React.createElement(App)));`;
        },
      },
    ],
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url, "http://fixture.local");
      if (!url.pathname.startsWith("/api/")) {
        if (url.pathname === "/check") {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/entity-fixture.tsx"></script>',
          );
          return;
        }
        return next();
      }
      requests.push({ method: req.method, path: url.pathname, at: Date.now() });
      const json = (value, status = 200) => {
        res.writeHead(status, {
          ...apiSchemaHeaders(),
          "Content-Type": "application/json",
        });
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/api/sync/stream") {
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        res.writeHead(200, {
          ...apiSchemaHeaders(),
          "Content-Type": "text/event-stream",
        });
        res.write(
          apiSchemaHandshakeSse(
            protocol3SseEvent("resources", {
              protocol: 4,
              workspaceId,
              epoch: "entity-fixture",
              revision: through,
              reason: "initial",
              resources,
              resourceVersions: [],
            }),
          ),
        );
        streams.add(res);
        res.on("close", () => streams.delete(res));
        return;
      }
      if (handleEntitySyncFixtureRequest(req, res, { snapshot, workspaceId }))
        return;
      if (url.pathname === "/api/session") return json({ token: "fixture" });
      if (url.pathname === "/api/accounts")
        return json({
          accounts: [
            {
              id: "default",
              provider: "codex",
              status: "ready",
              label: "Fixture",
            },
          ],
          archivedAccounts: [],
          defaultAccountKey: "default",
        });
      if (url.pathname === "/api/models") return json({ data: [] });
      if (
        url.pathname === "/api/limits" ||
        url.pathname === "/api/studio-errors"
      )
        return json({ error: "Unexpected automatic read" }, 500);
      if (url.pathname === "/api/multi-server")
        return json({
          identity: { serverId: "fixture", label: "Fixture" },
          servers: [],
        });
      if (url.pathname === "/api/ui-summary") return json({ system: "Linux" });
      if (url.pathname === "/api/queue") return json({ items: [] });
      if (url.pathname === "/api/terminals") return json({ items: [] });
      return json({ items: [], data: [], accounts: [], total: 0 });
    },
  });
  const publish = (changes) => {
    const from = through;
    for (const change of changes) change.seq = ++through;
    for (const stream of streams)
      stream.write(
        protocol3SseEvent("resources", {
          protocol: 4,
          workspaceId,
          epoch: "entity-fixture",
          revision: through,
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [
            {
              resource: { kind: "state" },
              revision: through,
              entityChanges: { after: from, through, documents: changes },
            },
          ],
        }),
      );
  };
  await server.listen();
  try {
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    try {
      await page.locator('[data-chat="lead"]').click({ timeout: 20_000 });
    } catch (error) {
      throw new Error(
        JSON.stringify({
          errors,
          requests,
          body: await page.locator("body").innerText(),
        }),
        { cause: error },
      );
    }
    await expect(page.locator("body")).toContainText("11%");
    const idleStart = requests.length;
    const item = {
      id: "agent:lead:turn",
      kind: "turn_disconnected",
      action: "resume",
      agentId: "lead",
      agentName: "Entity limits",
      reason: "The provider connection ended during the turn.",
      detail: "",
      subject: "",
      at: 100,
    };
    publish([
      {
        op: "upsert",
        collection: "rateLimit",
        id: "default",
        value: limits(37, 200),
      },
      { op: "upsert", collection: "studioError", id: item.id, value: item },
    ]);
    await expect(page.locator("body")).toContainText("37%");
    await expect(
      page.getByRole("button", { name: /Studio errors/ }),
    ).toBeVisible();
    await page.getByRole("button", { name: /Studio errors/ }).click();
    await expect(page.getByText(item.reason, { exact: true })).toBeVisible();
    publish([{ op: "delete", collection: "studioError", id: item.id }]);
    await expect(
      page.getByText("No agent work ended because of a Studio error."),
    ).toBeVisible();
    // Advance the old poll interval without spending two minutes of wall time.
    await page.clock.install();
    await page.clock.fastForward(125_000);
    const forbidden = requests.filter(
      ({ path }) => path === "/api/limits" || path === "/api/studio-errors",
    );
    assert.deepEqual(forbidden, []);
    assert.deepEqual(errors, []);
    const idle = requests.slice(idleStart);
    assert.equal(
      idle.filter(({ path }) => path === "/api/sync/pull").length,
      0,
      "complete events require no recovery pull",
    );
    await testInfo.attach("idle-http", {
      body: JSON.stringify({ requests, idle, forbidden }, null, 2),
      contentType: "application/json",
    });
  } finally {
    for (const stream of streams) stream.end();
    await server.close();
  }
});
