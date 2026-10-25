import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  createFixtureViteServer,
  entityPullFixtureForRequest,
  expect,
  handleEntitySyncFixtureRequest,
  protocol3SseEvent,
  test,
} from "../playwright.mjs";

test("selected Studio chat recovers after slow cursor preparation without reloading", async ({
  page,
}, testInfo) => {
  test.setTimeout(60_000);
  const workspaceId = "e".repeat(32);
  const snapshot = {
    chats: [],
    threads: [],
    edges: [],
    runtime: { agents: [] },
  };
  const streams = new Set();
  const sessions = new Map();
  const streamRequests = [];
  const reads = [];
  const errors = [];
  let recovering = false;
  let streamRevision = 0;
  let transcriptRevision = 1;
  const transcript = (id, revision = transcriptRevision) => ({
    items: [
      {
        id: `${id}-${revision}`,
        role: "assistant",
        text: `${id} revision ${revision}`,
      },
    ],
    truncated: false,
    agent: null,
    historyVersion: String(revision),
    nextAfterCursor: null,
    nextCursor: null,
    tail: null,
    unavailable: null,
  });
  const baseline = (resources) =>
    protocol3SseEvent("resources", {
      protocol: 4,
      workspaceId,
      epoch: "chat-switch-recovery",
      revision: ++streamRevision,
      reason: "initial",
      resources,
      resourceVersions: resources.map((resource) => ({
        resource,
        revision: transcriptRevision,
        cursor: { workspaceId, seq: transcriptRevision },
        replyKind: "snapshot",
        transcriptSnapshot: transcript(resource.agentId),
      })),
    });
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
    plugins: [
      {
        name: "studio-chat-switch-recovery-fixture",
        resolveId(id) {
          if (id === "virtual:studio-chat-switch") return "\0" + id;
        },
        load(id) {
          if (id !== "\0virtual:studio-chat-switch") return;
          return `
          import React, {useEffect,useState} from "react";
          import {createRoot} from "react-dom/client";
          import {useMessages} from "/src/hooks.ts";
          import {createServerServices} from "/src/servers/serverServices.ts";
          import {ServerServicesProvider} from "/src/servers/ServerServicesContext.tsx";
          const services = createServerServices({id:"local",label:"Fixture",origin:location.origin});
          await services.ready();
          window.services = services;
          window.cursorReads = [];
          window.connections = [];
          window.panelConnections = [];
          window.documentIdentity = crypto.randomUUID();
          const readCursor = services.sync.readSubscriptionCursor;
          services.sync.readSubscriptionCursor = async ref => {
            if (window.slowCursor) {
              window.cursorReads.push({ref,at:performance.now()});
              // Slow cache I/O crosses the real owner's three-second heartbeat.
              await new Promise(resolve => setTimeout(resolve,4000));
            }
            return readCursor(ref);
          };
          services.resources.watchResourceConnection((status,stream) => window.connections.push({status,stream,at:performance.now()}));
          function Panel({id,selected}) {
            const messages = useMessages(id,"agent",true,"fixture",${JSON.stringify(workspaceId)},selected);
            useEffect(() => {
              window.panelConnections.push({id,selected,connection:messages.connection,at:performance.now()});
            },[id,selected,messages.connection]);
            return React.createElement("section",{hidden:!selected,"data-chat":id},
              React.createElement("output",{"data-connection":true},messages.connection?.health ?? ""),
              React.createElement("div",{"data-messages":true},messages.items.map(item => item.text).join("\\n")));
          }
          function Fixture() {
            const [selected,setSelected] = useState("first");
            return React.createElement(React.Fragment,null,
              React.createElement("nav",{"aria-label":"Chats"},["first","second","third"].map(id =>
                React.createElement("button",{key:id,onClick:()=>setSelected(id)},id))),
              ["first","second","third"].map(id => React.createElement(Panel,{key:id,id,selected:selected===id})));
          }
          createRoot(document.getElementById("app")).render(
            React.createElement(ServerServicesProvider,{services},React.createElement(Fixture)));
        `;
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url, "http://fixture.invalid");
      if (
        url.pathname.startsWith("/api/") &&
        req.method === "GET" &&
        url.pathname !== "/api/sync/stream"
      )
        reads.push({
          path: url.pathname,
          scope: url.searchParams.get("scope"),
          recovering,
          at: Date.now(),
        });
      const json = (value) => {
        res.writeHead(
          200,
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        );
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") {
        res.setHeader("Content-Type", "text/html");
        res.end(
          '<!doctype html><div id="app"></div><script type="module" src="/@id/__x00__virtual:studio-chat-switch"></script>',
        );
        return;
      }
      if (url.pathname === "/api/session") return json({ token: "fixture" });
      if (
        url.pathname === "/api/sync/pull" &&
        url.searchParams.get("scope")?.startsWith("transcript:")
      ) {
        const id = url.searchParams.get("scope").slice("transcript:".length);
        return json({
          workspaceId,
          ...entityPullFixtureForRequest(snapshot, url, {
            // Recovery reads deliberately cannot provide the fresh stream value.
            transcript: transcript(id, 1),
            transcriptSeq: 1,
          }),
        });
      }
      if (url.pathname === "/api/sync/stream") {
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        streams.add(res);
        const sessionId = randomUUID();
        sessions.set(sessionId, res);
        res.once("close", () => {
          streams.delete(res);
          sessions.delete(sessionId);
        });
        streamRequests.push({
          resources,
          at: Date.now(),
        });
        res.writeHead(
          200,
          apiSchemaHeaders({
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
          }),
        );
        res.write(protocol3SseEvent("session", { sessionId }));
        res.write(apiSchemaHandshakeSse());
        res.write(baseline(resources));
        return;
      }
      if (url.pathname === "/api/sync/subscriptions" && req.method === "POST") {
        let body = "";
        req.on("data", (chunk) => {
          body += chunk;
        });
        req.on("end", () => {
          const update = JSON.parse(body);
          const stream = sessions.get(update.sessionId);
          if (!stream) {
            res.writeHead(410);
            res.end();
            return;
          }
          stream.write(protocol3SseEvent("subscriptions", { gen: update.gen }));
          if (update.add.length)
            stream.write(baseline(update.add.map((entry) => entry.resource)));
          json({ gen: update.gen });
        });
        return;
      }
      if (handleEntitySyncFixtureRequest(req, res, { snapshot, workspaceId }))
        return;
      next();
    },
  });
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const panel = (id) => page.locator(`section[data-chat="${id}"]`);
    const connection = (id) => panel(id).locator("[data-connection]");
    const messages = (id) => panel(id).locator("[data-messages]");
    await expect(connection("first")).toHaveText("live");
    await expect(messages("first")).toHaveText("first revision 1");
    const identity = await page.evaluate(() => window.documentIdentity);

    // A normal selection change keeps the server healthy and loads the new chat.
    const beforeSwitch = await page.evaluate(
      () => window.panelConnections.length,
    );
    await page.getByRole("button", { name: "second", exact: true }).click();
    await expect(connection("second")).toHaveText("live");
    await expect(messages("second")).toHaveText("second revision 1");
    assert.ok(
      await page.evaluate(
        (before) =>
          window.panelConnections
            .slice(before)
            .every(
              (event) =>
                !event.selected ||
                (event.connection?.health !== "degraded" &&
                  event.connection?.health !== "offline"),
            ),
        beforeSwitch,
      ),
    );
    await page.getByRole("button", { name: "first", exact: true }).click();
    await expect(connection("first")).toHaveText("live");

    await page.evaluate(() => {
      window.slowCursor = true;
    });
    transcriptRevision++;
    recovering = true;
    const beforeRecovery = streamRequests.length;
    for (const response of streams) response.end();
    await expect(connection("first")).toHaveText("degraded");
    await page.waitForFunction(() => window.cursorReads.length > 0);
    // Select a previously unopened chat while the old resource union is preparing.
    await page.getByRole("button", { name: "third", exact: true }).click();
    await expect(connection("third")).toHaveText("degraded");
    await expect(connection("third")).toHaveText("live", { timeout: 12_000 });
    await expect(messages("third")).toHaveText("third revision 2");
    const recoveryPulls = reads.filter(
      (read) => read.recovering && read.path === "/api/sync/pull",
    );
    assert.ok(
      recoveryPulls.length <= 1 &&
        recoveryPulls.every((read) => read.scope === "transcript:third"),
      "only the newly opened chat may bootstrap from HTTP while degraded; confirmed chats wait for the stream baseline",
    );
    assert.equal(await page.evaluate(() => window.documentIdentity), identity);
    assert.ok(
      streamRequests
        .slice(beforeRecovery)
        .some(({ resources }) =>
          resources.some(
            (ref) => ref.kind === "transcript" && ref.agentId === "third",
          ),
        ),
      "the replacement stream includes the currently selected chat",
    );
    assert.deepEqual(errors, []);
  } finally {
    const browserEvidence = await page
      .evaluate(() => ({
        cursorReads: window.cursorReads,
        connections: window.connections,
        panelConnections: window.panelConnections,
      }))
      .catch(() => ({}));
    await testInfo.attach("chat-switch-recovery.json", {
      body: JSON.stringify(
        { streamRequests, reads, errors, ...browserEvidence },
        null,
        2,
      ),
      contentType: "application/json",
    });
    await page.evaluate(() => window.services?.close()).catch(() => {});
    await page.close();
    for (const response of streams) response.end();
    await server.close();
  }
});
