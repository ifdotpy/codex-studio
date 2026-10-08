import {
  test,
  expect,
  handleEntitySyncFixtureRequest,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
} from "../playwright.mjs";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import crypto from "node:crypto";
import { execFileSync } from "node:child_process";
const root = fileURLToPath(new URL("../../../", import.meta.url));
const workspaceId = "b".repeat(32);
async function fixture(label, signed = false) {
  const keys = crypto.generateKeyPairSync("ed25519");
  const publicKey = keys.publicKey
    .export({ format: "pem", type: "spki" })
    .toString();
  const serverId = label.toLowerCase();
  const serve = `https://${serverId}.tailnet.ts.net`;
  const invitation = {
    protocol: 1,
    inviteId: "invite-" + serverId,
    token: "secret-" + serverId,
    serverId,
    label,
    origin: serve,
    publicKey,
    tailscaleUser: "owner",
    expires: Math.floor(Date.now() / 1000) + 900,
  };
  const clients = new Map();
  const accessClients = [
    { clientId: "old-ui", label: "Old UI", status: "paired" },
  ];
  const writes = [];
  const pairs = [];
  const streams = new Set();
  const nonces = new Set();
  const notify = [];
  const agent = {
    id: "overlap",
    name: `${label} chat`,
    isLead: true,
    rootId: "overlap",
    source: "managed",
    cwd: "/same/project",
    status: "waiting",
    autoWake: true,
    canSend: true,
    model: "fixture-model",
    accountKey: "default",
    inFlight: false,
    archived: false,
    deletedAt: null,
  };
  const snapshot = {
    token: "local-only",
    stateDir: "/same/state",
    chats: [],
    edges: [],
    threads: [agent],
    runtime: {
      agents: [agent],
      projects: [
        {
          path: "/same/project",
          name: `${label} project`,
          id: "/same/project",
          folders: [],
          peerTeams: [],
        },
      ],
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      peerTeams: [],
      requests: [],
      rules: [],
      events: [],
      work: [],
      nativeNotices: [],
      sidebarOrder: null,
      rateLimits: null,
      rateLimitsByAccount: {},
    },
  };
  let offline = false;
  const server = createServer(async (request, response) => {
    response.setHeader("Access-Control-Allow-Origin", "*");
    response.setHeader("Access-Control-Allow-Headers", "*");
    response.setHeader("Access-Control-Expose-Headers", "*");
    if (request.method === "OPTIONS") {
      response.writeHead(204);
      response.end();
      return;
    }
    if (offline && request.url.startsWith("/api/")) {
      response.writeHead(503);
      response.end("{}");
      return;
    }
    const url = new URL(request.url, "http://fixture.invalid");
    const json = (value, status = 200) => {
      response.writeHead(status, {
        "Content-Type": "application/json",
        [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      });
      response.end(JSON.stringify(value));
    };
    let bytes = Buffer.alloc(0);
    for await (const chunk of request) bytes = Buffer.concat([bytes, chunk]);
    const body = bytes.length ? JSON.parse(bytes.toString()) : {};
    if (signed && url.pathname.startsWith("/api/")) {
      const h = request.headers;
      const ownKey = url.pathname.endsWith("/pair")
        ? body.publicKey
        : clients.get(h["x-studio-client"]);
      const canonical = [
        "studio-multi-server-v1",
        request.method,
        url.pathname + url.search,
        serverId,
        h["x-studio-client"],
        h["x-studio-timestamp"],
        h["x-studio-nonce"],
        h["x-studio-request-id"],
        crypto.createHash("sha256").update(bytes).digest("hex"),
      ].join("\n");
      if (
        !ownKey ||
        h["x-studio-server"] !== serverId ||
        h["x-canvas-token"] ||
        nonces.has(h["x-studio-nonce"]) ||
        !crypto.verify(
          null,
          Buffer.from(canonical),
          ownKey,
          Buffer.from(h["x-studio-signature"] || "", "base64"),
        )
      ) {
        json({ error: "Invalid signature" }, 401);
        return;
      }
      nonces.add(h["x-studio-nonce"]);
      if (body.requestId || body.request_id)
        expect(h["x-studio-request-id"]).toBe(
          body.requestId || body.request_id,
        );
    }
    if (url.pathname === "/api/multi-server/v1/pair") {
      pairs.push(bytes.toString());
      clients.set(body.clientId, body.publicKey);
      json({ ...invitation, clientId: body.clientId, paired: true });
      return;
    }
    if (url.pathname === "/api/sync/stream") {
      streams.add(response);
      response.on("close", () => streams.delete(response));
    }
    if (url.pathname === "/api/sync/drafts") {
      json([]);
      return;
    }
    if (
      handleEntitySyncFixtureRequest(request, response, {
        snapshot,
        workspaceId,
        onStreamReady: (send) => notify.push(send),
      })
    )
      return;
    if (url.pathname === "/api/multi-server") {
      if (body.action === "create_invite") {
        json({ invitation, expires: invitation.expires });
        return;
      }
      if (body.action === "revoke") accessClients[0].status = "revoked";
      json({
        protocol: 1,
        identity: invitation,
        clients: accessClients,
        servers: [],
        invites: [],
      });
      return;
    }
    if (url.pathname === "/api/session") {
      json({ token: snapshot.token });
      return;
    }
    if (url.pathname === "/api/state") {
      json(snapshot);
      return;
    }
    if (url.pathname === "/api/accounts") {
      json({
        accounts: [
          {
            id: "default",
            label: "Fixture",
            provider: "codex",
            status: "ready",
          },
        ],
        defaultAccountKey: "default",
        archivedAccounts: [],
        logins: [],
      });
      return;
    }
    if (url.pathname === "/api/models") {
      json({
        models: [
          {
            model: "fixture-model",
            displayName: "Fixture",
            supportedReasoningEfforts: [],
          },
        ],
      });
      return;
    }
    if (url.pathname === "/api/messages") {
      writes.push(body);
      json({ id: body.id, status: "queued" });
      return;
    }
    if (url.pathname === "/api/search") {
      json({
        results: [
          {
            id: "result-" + serverId,
            kind: "message",
            agent: "overlap",
            room: "",
            text: `${label}: ${url.searchParams.get("q")}`,
          },
        ],
      });
      return;
    }
    if (url.pathname.startsWith("/api/")) {
      json({
        items: [],
        queues: [],
        accounts: [],
        models: [],
        token: snapshot.token,
      });
      return;
    }
    try {
      const filename = path.join(
        root,
        "web/dist",
        url.pathname === "/" ? "index.html" : url.pathname,
      );
      const file = await readFile(filename);
      response.setHeader(
        "Content-Type",
        filename.endsWith(".js")
          ? "text/javascript"
          : filename.endsWith(".css")
            ? "text/css"
            : "text/html",
      );
      if (url.pathname === "/")
        response.setHeader(
          "Content-Security-Policy",
          "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' http://127.0.0.1:* https://api.openai.com https://*.ts.net; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob:; frame-ancestors " +
            (url.searchParams.has("studio-server") ? "'self'" : "'none'") +
            "; base-uri 'none'",
        );
      response.end(file);
    } catch {
      response.writeHead(404);
      response.end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    origin: `http://127.0.0.1:${server.address().port}`,
    invitation,
    writes,
    pairs,
    complete() {
      Object.assign(agent, {
        threadId: "thread-overlap",
        status: "completed",
        lastCompletedTurn: "turn-completed",
        lastCompletedTurnStatus: "completed",
        tail: `${label} reply`,
      });
      notify.forEach((send) => send([{ kind: "state" }]));
    },
    offline(value) {
      offline = value;
      if (value) for (const stream of streams) stream.destroy();
    },
    async close() {
      for (const stream of streams) stream.destroy();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}
test("one UI routes overlapping chats to two and three signed servers without reload", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(120000);
  page.setDefaultTimeout(15000);
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  const third = await fixture("Third", true);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await context.addInitScript(
      ({ destinations }) => {
        window.__studioNotifications = [];
        window.Notification = class {
          static permission = "granted";
          constructor(title, options) {
            this.title = title;
            this.options = options;
            window.__studioNotifications.push(this);
          }
          close() {}
        };
        const nativeFetch = window.fetch.bind(window);
        // A test-only network shim keeps HTTPS identities while the fixture uses
        // isolated loopback ports. All signatures are verified by the HTTP fixture.
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (!destinations[url.origin]) return nativeFetch(request);
          const bytes = await request.clone().arrayBuffer();
          const response = await nativeFetch(
            destinations[url.origin] + url.pathname + url.search,
            {
              method: request.method,
              headers: request.headers,
              ...(bytes.byteLength ? { body: bytes } : {}),
              signal: request.signal,
              redirect: "error",
              credentials: "omit",
            },
          );
          if (
            url.pathname.endsWith("/pair") &&
            url.hostname.startsWith("remote.") &&
            !sessionStorage.getItem("studio-test-pair-response-lost")
          ) {
            sessionStorage.setItem("studio-test-pair-response-lost", "1");
            await response.text();
            throw new TypeError("Pair response lost in fixture");
          }
          return response;
        };
      },
      {
        destinations: {
          [remote.invitation.origin]: remote.origin,
          [third.invitation.origin]: third.origin,
        },
      },
    );
    await page.goto(local.origin);
    await page.locator("#message").waitFor();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Server access", exact: true }).click();
    const access = page.getByRole("region", {
      name: "Server access",
      exact: true,
    });
    await access
      .getByRole("button", { name: "Create pairing invitation", exact: true })
      .click();
    await expect(access.getByLabel("Pairing invitation")).toHaveValue(
      JSON.stringify(local.invitation, null, 2),
    );
    await access
      .getByRole("button", { name: "Revoke access", exact: true })
      .click();
    await expect(
      access.getByRole("button", { name: "Revoke access", exact: true }),
    ).toBeDisabled();
    await page
      .getByRole("dialog")
      .filter({
        has: page.getByRole("heading", {
          name: "Studio settings",
          exact: true,
        }),
      })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    const pair = async (server) => {
      const dialog = page.getByRole("dialog", { name: "Studio servers" });
      await expect(dialog).toBeVisible();
      await dialog.getByLabel("Server address").fill(server.invitation.origin);
      await dialog
        .getByLabel("Pairing invitation")
        .fill(JSON.stringify(server.invitation));
      await dialog
        .getByRole("button", { name: "Pair server", exact: true })
        .click();
      await expect(
        page.locator(
          `[data-server="${server.invitation.serverId}"] .server-chat`,
        ),
      ).toContainText(`${server.invitation.label} chat`);
      await dialog.getByRole("button", { name: "Close", exact: true }).click();
    };
    await page.getByRole("button", { name: "Servers", exact: true }).click();
    const firstPair = page.getByRole("dialog", { name: "Studio servers" });
    await expect(firstPair).toBeVisible();
    await firstPair.getByLabel("Server address").fill(remote.invitation.origin);
    await firstPair
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await firstPair
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    await expect(firstPair.getByRole("alert")).toHaveText(
      "Pair response lost in fixture",
    );
    await page.reload();
    await page.getByRole("button", { name: "Servers", exact: true }).click();
    await pair(remote);
    expect(remote.pairs).toHaveLength(2);
    expect(remote.pairs[0]).toBe(remote.pairs[1]);
    const cdp = await context.newCDPSession(page);
    await cdp.send("Performance.enable");
    const browserCdp = await context.browser().newBrowserCDPSession();
    const measurements = [];
    for (const count of [2, 3]) {
      if (count === 3) {
        await page.getByRole("button", { name: "Manage", exact: true }).click();
        await pair(third);
      }
      const start = Date.now();
      await page.reload();
      await expect(
        page.locator(".server-sidebar [data-chat='overlap']"),
      ).toHaveCount(count);
      const startupMs = Date.now() - start;
      const metric = await cdp.send("Performance.getMetrics");
      const heap = metric.metrics.find(
        (row) => row.name === "JSHeapUsedSize",
      ).value;
      const processes = await browserCdp.send("SystemInfo.getProcessInfo");
      const ids = processes.processInfo
        .map((row) => row.id)
        .filter((id) => Number.isInteger(id) && id > 0);
      const rss =
        execFileSync("ps", ["-o", "rss=", "-p", ids.join(",")], {
          encoding: "utf8",
        })
          .trim()
          .split(/\s+/)
          .reduce((sum, value) => sum + Number(value), 0) * 1024;
      measurements.push({
        servers: count,
        startupMs,
        jsHeapUsedBytes: heap,
        browserResidentBytes: rss,
        frames: page.frames().length,
      });
    }
    await page.locator('[data-server="local"] .server-chat').click();
    const localFrame = page.frameLocator(
      'iframe[title="Studio on This computer"]',
    );
    const remoteFrame = page.frameLocator('iframe[title="Studio on Remote"]');
    const timeOrigins = await Promise.all(
      page
        .frames()
        .filter((frame) => frame !== page.mainFrame())
        .map((frame) => frame.evaluate(() => performance.timeOrigin)),
    );
    await localFrame.locator("#message").fill("local draft");
    await page.locator('[data-server="remote"] .server-chat').click();
    await remoteFrame.locator("#message").fill("remote draft");
    await page.locator('[data-server="local"] .server-chat').click();
    await expect(localFrame.locator("#message")).toHaveValue("local draft");
    await page.locator('[data-server="remote"] .server-chat').click();
    await expect(remoteFrame.locator("#message")).toHaveValue("remote draft");
    expect(
      await Promise.all(
        page
          .frames()
          .filter((frame) => frame !== page.mainFrame())
          .map((frame) => frame.evaluate(() => performance.timeOrigin)),
      ),
    ).toEqual(timeOrigins);
    await remoteFrame
      .getByRole("button", { name: "Send message", exact: true })
      .click();
    await expect.poll(() => remote.writes.length).toBe(1);
    expect(local.writes).toHaveLength(0);
    expect(remote.writes[0].text).toBe("remote draft");
    await page
      .getByRole("button", { name: "Search all messages", exact: true })
      .click();
    const search = page.getByRole("dialog", { name: "Search all servers" });
    await search
      .getByLabel("Search all messages", { exact: true })
      .fill("shared term");
    await search.getByRole("button", { name: "Search", exact: true }).click();
    await expect(search.locator(".server-search-result")).toHaveCount(3);
    await search.getByRole("button", { name: "Close", exact: true }).click();
    remote.offline(true);
    await expect(
      page.locator('[data-server="remote"] [role="status"]'),
    ).toHaveText("Offline");
    await expect(remoteFrame.locator("#message")).toBeVisible();
    remote.offline(false);
    await expect(
      page.locator('[data-server="remote"] [role="status"]'),
    ).toHaveText("Online", { timeout: 20000 });
    local.complete();
    third.complete();
    await expect(
      page.getByLabel("2 unread chats", { exact: true }),
    ).toBeVisible();
    await expect
      .poll(() => page.evaluate(() => window.__studioNotifications.length))
      .toBe(2);
    expect(
      await page.evaluate(() =>
        window.__studioNotifications.map((notice) => notice.title).sort(),
      ),
    ).toEqual([
      "Third: Third chat: Reply ready",
      "This computer: Local chat: Reply ready",
    ]);
    await remoteFrame.locator("#message").press("Control+k");
    await expect(
      page.getByRole("dialog", { name: "Search all servers" }),
    ).toBeVisible();
    await page
      .getByRole("dialog", { name: "Search all servers" })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await page
      .locator('[data-server="remote"] .server-tools')
      .getByRole("button", { name: "Settings", exact: true })
      .click();
    await remoteFrame
      .getByRole("tab", { name: "Appearance", exact: true })
      .click();
    await remoteFrame
      .getByLabel("Studio theme", { exact: true })
      .selectOption("dark");
    await remoteFrame
      .getByRole("dialog")
      .filter({
        has: remoteFrame.getByRole("heading", {
          name: "Studio settings",
          exact: true,
        }),
      })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await expect(page.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await expect(localFrame.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await expect(remoteFrame.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await page.setViewportSize({ width: 600, height: 800 });
    await page
      .getByRole("button", { name: "Servers and chats", exact: true })
      .click();
    await expect(
      page.getByRole("complementary", { name: "Servers and projects" }),
    ).toBeVisible();
    await page.locator('[data-server="remote"] .server-chat').click();
    await expect(
      page.getByRole("complementary", { name: "Servers and projects" }),
    ).not.toBeVisible();
    await remoteFrame
      .getByRole("button", { name: "Toggle conversations", exact: true })
      .click();
    await expect(
      page.getByRole("complementary", { name: "Servers and projects" }),
    ).toBeVisible();
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.screenshot({
      path: testInfo.outputPath("multi-server-ui.png"),
      fullPage: true,
    });
    expect(errors).toEqual([]);
    await testInfo.attach("multi-server-measurements.json", {
      body: JSON.stringify(measurements),
      contentType: "application/json",
    });
    console.log(JSON.stringify({ multiServerMeasurements: measurements }));
  } finally {
    await page.goto("about:blank").catch(() => {});
    await Promise.all([local.close(), remote.close(), third.close()]);
  }
});
