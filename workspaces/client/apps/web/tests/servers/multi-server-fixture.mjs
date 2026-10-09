import assert from "node:assert/strict";
import {
  handleEntitySyncFixtureRequest,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
} from "../playwright.mjs";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import crypto from "node:crypto";
const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
const workspaceId = "b".repeat(32);
export async function fixture(
  label,
  signed = false,
  accountRowsOrOptions = null,
) {
  const options = Array.isArray(accountRowsOrOptions)
    ? {}
    : accountRowsOrOptions || {};
  const accountRows = Array.isArray(accountRowsOrOptions)
    ? accountRowsOrOptions
    : null;
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
  const accountWrites = [];
  const accountLogins = new Map();
  const savedAccounts = accountRows || [
    {
      id: "default",
      label: "Fixture",
      provider: "codex",
      status: "ready",
    },
  ];
  const accessRequests = [];
  const aliases = { local: "MAC", [serverId]: "MAC" };
  let discoveredPeers = [];
  let autoPair = true;
  let staleSettingsReceipts = false;
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
    response.setHeader(
      "Access-Control-Expose-Headers",
      "X-Studio-Server, X-Studio-API-Schema, X-Studio-API-Schema-Mismatch, ETag, Content-Disposition, X-Log-Truncated",
    );
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
      if (body.requestId || body.request_id || body.login_id) {
        if (url.pathname.startsWith("/api/accounts/")) {
          accountWrites.push({
            path: url.pathname,
            requestId: h["x-studio-request-id"],
            loginId: body.login_id || body.request_id,
            code: body.code,
          });
          assert.notEqual(
            h["x-studio-request-id"],
            body.login_id || body.request_id,
          );
        } else
          assert.equal(
            h["x-studio-request-id"],
            body.requestId || body.request_id,
          );
      }
    }
    if (options.handle?.({ request, url, body, json, snapshot })) return;
    if (url.pathname === "/api/monitor/log") {
      response.writeHead(200, {
        "Content-Type": "text/plain",
        "Content-Disposition": 'attachment; filename="remote-monitor.log"',
        "X-Log-Truncated": "true",
      });
      response.end("remote log");
      return;
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
    if (url.pathname === "/api/ui-summary") {
      const unread = !!(
        agent.threadId &&
        agent.lastCompletedTurn &&
        agent.lastCompletedTurnStatus === "completed"
      );
      json({
        ready: true,
        busy: !!agent.inFlight,
        system: "Darwin",
        agentsRunning: agent.inFlight ? 1 : 0,
        projects: snapshot.runtime.projects,
        chats: snapshot.threads
          .filter(
            (chat) =>
              chat.source === "managed" &&
              chat.isLead &&
              !chat.deletedAt &&
              !chat.sharedRoomId,
          )
          .map((chat) => ({
            id: chat.id,
            name: chat.name,
            path: chat.cwd || "",
            archived: !!chat.archived,
            status: chat.status || "",
            unread: chat.id === agent.id ? unread : false,
            serverId: chat.serverId || null,
            projectId: chat.projectId || null,
            projectServerId: chat.projectServerId || null,
            provider: chat.provider || null,
            updated: chat.updated || null,
            created: chat.created || null,
            inFlight: !!chat.inFlight,
            pinned: !!chat.pinned,
            team: false,
          })),
        alerts: unread
          ? [
              {
                id: `completed:${agent.id}:${agent.lastCompletedTurn}`,
                title: `${agent.name}: Reply ready`,
                body: agent.tail,
                target: { agentId: agent.id, section: "messages" },
              },
            ]
          : [],
        accounts: savedAccounts.map((account) => ({
          provider: account.provider || "codex",
          email: account.email || null,
          plan: account.plan || null,
          status: account.disconnected ? "signedOut" : account.status,
          label: account.label || "Account",
          isDefault: !!account.isDefault,
        })),
      });
      return;
    }
    if (url.pathname === "/api/multi-server/v1/status") {
      json(invitation);
      return;
    }
    if (url.pathname === "/api/multi-server") {
      if (request.method === "POST") accessRequests.push(body);
      if (
        ["ui_invite", "discover", "settings", "alias", "name"].includes(
          body.action,
        )
      )
        if (body.action !== "name" || !request.headers["x-studio-client"])
          assert.equal(request.headers["x-canvas-token"], snapshot.token);
      if (body.action === "name") {
        if (body.serverId === "local" || body.serverId === serverId)
          invitation.label = body.label;
        else {
          const peer = discoveredPeers.find(
            (row) => row.serverId === body.serverId,
          );
          if (!peer) {
            json({ error: "Server not paired" }, 403);
            return;
          }
          peer.label = peer.invitation.label = body.label;
        }
      }
      if (body.action === "alias") {
        aliases[body.serverId] = body.alias;
        if (body.serverId === "local" || body.serverId === serverId)
          aliases.local = aliases[serverId] = body.alias;
      }
      if (body.action === "ui_invite") {
        const target = discoveredPeers.find(
          (row) => row.serverId === body.serverId,
        );
        if (!target || target.status !== "paired") {
          json({ error: "Server not paired" }, 403);
          return;
        }
        json({
          invitation: target.invitation,
          expires: target.invitation.expires,
        });
        return;
      }
      if (body.action === "settings") autoPair = body.autoPair;
      if (body.action === "create_invite") {
        json({ invitation, expires: invitation.expires });
        return;
      }
      if (body.action === "revoke") {
        const peer = discoveredPeers.find(
          (row) => row.clientId === body.clientId,
        );
        if (peer) peer.status = "revoked";
        else accessClients[0].status = "revoked";
      }
      if (body.action === "unrevoke") {
        const peer = discoveredPeers.find(
          (row) => row.clientId === body.clientId,
        );
        if (peer) {
          peer.status = "discovered";
          peer.created += 1;
        }
      }
      json({
        protocol: 1,
        identity: invitation,
        clients: accessClients,
        // An exact retry can return an earlier snapshot. GET stays current.
        settings: {
          aliases,
          autoPair:
            staleSettingsReceipts && body.action === "settings"
              ? !autoPair
              : autoPair,
        },
        servers: discoveredPeers.map(
          ({ invitation: _invitation, ...peer }) => peer,
        ),
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
        accounts: savedAccounts.map((account) => ({
          ...account,
          isDefault: account.isDefault || false,
        })),
        defaultAccountKey:
          savedAccounts.find((account) => account.isDefault)?.id || "default",
        archivedAccounts: [],
        logins: [...accountLogins.values()],
      });
      return;
    }
    if (url.pathname === "/api/accounts/claude/add") {
      const id = body.login_id;
      const receipt = {
        requestId: id,
        accountKey: id,
        status: "pending",
        verificationUrl: "https://claude.com/cai/oauth/authorize?state=fake",
      };
      accountLogins.set(id, receipt);
      json(receipt);
      return;
    }
    if (url.pathname === "/api/accounts/claude/login") {
      const receipt = accountLogins.get(url.searchParams.get("request_id"));
      json(
        receipt || { error: "Unknown Claude sign-in request" },
        receipt ? 200 : 404,
      );
      return;
    }
    if (url.pathname === "/api/accounts/claude/login/code") {
      const previous = accountLogins.get(body.login_id);
      const receipt = {
        ...previous,
        status: "ready",
        email: previous?.email || "new@example.test",
        plan: "Max",
      };
      accountLogins.set(body.login_id, receipt);
      savedAccounts.push({
        id: body.login_id,
        label: "New Claude",
        provider: "claude",
        email: receipt.email,
        plan: receipt.plan,
        status: "ready",
        isDefault: false,
      });
      json(receipt);
      return;
    }
    if (url.pathname === "/api/accounts/claude/login/cancel") {
      const previous = accountLogins.get(body.login_id);
      const receipt = { ...previous, status: "cancelled" };
      accountLogins.set(body.login_id, receipt);
      json(receipt);
      return;
    }
    if (url.pathname === "/api/accounts/login") {
      const id = body.login_id;
      const account = savedAccounts.find((row) => row.id === body.account_key);
      const receipt = account
        ? {
            requestId: id,
            accountKey: account.id,
            resolvedAccountKey: account.id,
            reauthAccountKey: account.id,
            status: "ready",
            email: account.email,
          }
        : {
            requestId: id,
            accountKey: id,
            loginId: `fake-${id}`,
            status: "pending",
            userCode: "FAKE-CODE",
            verificationUrl: "https://auth.openai.com/codex/device",
            expiresAt: Date.now() / 1000 + 90,
          };
      accountLogins.set(id, receipt);
      json(receipt);
      return;
    }
    if (url.pathname === "/api/accounts/login/cancel") {
      const previous = accountLogins.get(body.login_id);
      const receipt = { ...previous, status: "cancelled" };
      accountLogins.set(body.login_id, receipt);
      json(receipt);
      return;
    }
    if (url.pathname === "/api/accounts/reconnect") {
      const account = savedAccounts.find((row) => row.id === body.account_key);
      if (account) {
        account.disconnected = false;
        account.status = "ready";
      }
      json({
        accounts: savedAccounts,
        defaultAccountKey:
          savedAccounts.find((row) => row.isDefault)?.id || "default",
        archivedAccounts: [],
        logins: [...accountLogins.values()],
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
      writes.push({ ...body, _canvasToken: request.headers["x-canvas-token"] });
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
        "workspaces/client/apps/web/dist",
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
          "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' http://127.0.0.1:* https://api.openai.com https://*.ts.net:*; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; frame-ancestors " +
            (url.searchParams.has("studio-server")
              ? "'self' http://127.0.0.1:* http://localhost:*"
              : "'none'") +
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
    snapshot,
    writes,
    accountWrites,
    accounts() {
      return savedAccounts;
    },
    pairs,
    accessRequests,
    aliases,
    setChats(rows) {
      snapshot.threads = rows;
      snapshot.runtime.agents = rows;
    },
    staleSettingsReceipts(value) {
      staleSettingsReceipts = value;
    },
    discoverPeer(peer, status = "paired", reachability) {
      const value = peer.invitation;
      discoveredPeers = [
        ...discoveredPeers.filter((row) => row.serverId !== value.serverId),
        {
          id: value.serverId,
          clientId: value.serverId,
          serverId: value.serverId,
          kind: "server",
          label: value.label,
          origin: value.origin,
          publicKey: value.publicKey,
          tailscaleUser: value.tailscaleUser,
          status,
          reachability,
          created: 1,
          lastSeen: Math.floor(Date.now() / 1000),
          autoPair: true,
          invitation: value,
        },
      ];
    },
    complete(turnId = "turn-completed") {
      Object.assign(agent, {
        threadId: "thread-overlap",
        status: "completed",
        lastCompletedTurn: turnId,
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
      await new Promise((resolve) => {
        server.close(resolve);
        server.closeAllConnections();
      });
    },
  };
}
