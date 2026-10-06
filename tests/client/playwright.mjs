import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";

const require = createRequire(
  fileURLToPath(new URL("../../web/package.json", import.meta.url)),
);
const { test: baseTest, expect } = require("@playwright/test");
const { chromium } = require("playwright");

const generatedApiSchema = readFileSync(
  new URL("../../web/src/generated/apiSchema.ts", import.meta.url),
  "utf8",
);

export const API_SCHEMA_HASH_HEADER = readGeneratedString(
  "API_SCHEMA_HASH_HEADER",
);
export const API_SCHEMA_HASH_PARAM = readGeneratedString(
  "API_SCHEMA_HASH_PARAM",
);

function readGeneratedString(name) {
  const match = generatedApiSchema.match(
    new RegExp(`${name}\\s*=\\s*"([^"]+)"`),
  );
  if (!match) throw new Error(`Generated ${name} is missing.`);
  return match[1];
}

export function readApiSchemaHash() {
  const match = generatedApiSchema.match(
    /API_SCHEMA_HASH\s*=\s*"([0-9a-f]{64})"/,
  );
  if (!match) throw new Error("Generated API schema hash is missing.");
  return match[1];
}

export function apiSchemaHandshakeSse(body = "", overrides = {}) {
  return `event: api-schema\ndata: ${JSON.stringify(apiSchemaHandshakeEvent(overrides))}\n\n${body}`;
}

export function apiSchemaHandshakeEvent(overrides = {}) {
  return { hash: readApiSchemaHash(), ...overrides };
}

export function protocol3SseEvent(name, value) {
  return `event: ${name}\ndata: ${JSON.stringify(value)}\n\n`;
}

const fixtureScopes = new WeakMap();
const childTerminationWaitMs = 1_000;
const fixtureCleanupTimeoutMs = 5_000;

async function waitForChildExit(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      child.off("close", onClose);
      resolve();
    }, childTerminationWaitMs);
    timer.unref?.();
    const onClose = () => {
      clearTimeout(timer);
      resolve();
    };
    child.once("close", onClose);
  });
}

async function waitForChildStart(child) {
  if (
    child.pid !== undefined ||
    child.exitCode !== null ||
    child.signalCode !== null
  )
    return;
  await new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      child.off("spawn", finish);
      child.off("error", finish);
      child.off("close", finish);
      resolve();
    };
    const timer = setTimeout(finish, childTerminationWaitMs);
    timer.unref?.();
    child.once("spawn", finish);
    child.once("error", finish);
    child.once("close", finish);
  });
}

async function stopFixtureChild(child) {
  await waitForChildStart(child);
  if (child.pid === undefined) return;
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill("SIGTERM");
  await waitForChildExit(child);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill("SIGKILL");
    await waitForChildExit(child);
  }
  if (child.exitCode === null && child.signalCode === null)
    throw new Error(`Fixture process ${child.pid} did not exit after SIGKILL`);
}

const test = baseTest.extend({
  fixtureChildCleanup: [
    // oxlint-disable-next-line no-empty-pattern
    async ({}, use, testInfo) => {
      const scope = { children: new Set(), errors: [], closing: false };
      fixtureScopes.set(testInfo, scope);
      let testFailed = false;
      let testError;
      try {
        await use();
      } catch (error) {
        testFailed = true;
        testError = error;
      }
      scope.closing = true;
      const results = await Promise.allSettled(
        [...scope.children].map(stopFixtureChild),
      );
      fixtureScopes.delete(testInfo);
      const cleanupFailures = [
        ...scope.errors,
        ...results
          .filter((result) => result.status === "rejected")
          .map((result) => result.reason),
      ];
      if (cleanupFailures.length)
        throw new AggregateError(
          testFailed ? [testError, ...cleanupFailures] : cleanupFailures,
          "Fixture child process failed or cleanup did not complete",
        );
      if (testFailed) throw testError;
    },
    { auto: true, timeout: fixtureCleanupTimeoutMs },
  ],
});

export { test, expect };

export function spawnFixture(command, args, options) {
  let testInfo;
  try {
    testInfo = baseTest.info();
  } catch {
    throw new Error(
      "spawnFixture must be called during a shared Playwright test",
    );
  }
  const scope = fixtureScopes.get(testInfo);
  if (!scope || scope.closing)
    throw new Error(
      "spawnFixture is only available before test cleanup begins",
    );

  const child = spawn(command, args, options);
  scope.children.add(child);
  child.on("error", (error) => {
    if (error.code !== "ESRCH") scope.errors.push(error);
  });
  child.once("close", () => scope.children.delete(child));
  return child;
}

export const browserExecutablePath =
  process.env.CHROME_BIN?.trim() || chromium.executablePath();
// @ts-check

/** @typedef {import("../../web/src/generated/api").components["schemas"]["JsonValue"]} JsonValue */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SessionResponse"]} SessionResponse */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SyncPullResponse"]} SyncPullResponse */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SyncPullResetResponse"]} SyncPullResetResponse */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SyncIdentityResponse"]} SyncIdentityResponse */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SyncProtocolResponse"]} SyncProtocolResponse */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["StateSnapshot"]} StateSnapshot */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotAgentDto"]} SnapshotAgentDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotRoomDto"]} SnapshotRoomDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotTaskDto"]} SnapshotTaskDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotProjectDto"]} SnapshotProjectDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotComplaintDto"]} SnapshotComplaintDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["AgentOverview"]} AgentOverview */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["RuntimeSnapshot"]} RuntimeSnapshot */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["RuleSnapshotDto"]} RuleSnapshotDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["WorkSnapshotDto"]} WorkSnapshotDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotChatGroupDto"]} SnapshotChatGroupDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["SnapshotEdgeDto"]} SnapshotEdgeDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["EventEntityDto"]} EventEntityDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["MonitorEntityDto"]} MonitorEntityDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["PeerTeamEntityDto"]} PeerTeamEntityDto */
/** @typedef {import("../../web/src/generated/api").components["schemas"]["RequestEntityDto"]} RequestEntityDto */

/**
 * Keep this view limited to fields in AgentEntityDto. Snapshot-only fields are
 * intentionally available only from readLegacySnapshotForS2Assertions().
 * @typedef {Pick<SnapshotAgentDto,
 *   "id" | "name" | "manualName" | "status" | "source" | "kind" |
 *   "parentId" | "rootId" | "threadId" | "orchestratorId" | "orchestratorName" |
 *   "isLead" | "role" | "sharedRoomId" | "model" | "provider" | "effort" |
 *   "fastMode" | "concurrency" | "accountKey" | "cwd" | "worktree" |
 *   "worktreePreparation" | "imageWorkspace" | "imageWorkspaceReady" |
 *   "imageWorkspacePhase" | "imageWorkspaceError" | "imageWorkspaceRepo" |
 *   "imageWorkspaceBaseRepo" | "imageWorkspaceRelative" | "imageWorkspaceStartCommit" |
 *   "created" | "updated" | "turnId" | "turnStatus" | "inFlight" | "compactions" |
 *   "tokensUsed" | "contextUsage" | "error" | "tail" | "canSend" | "launcherAlive" |
 *   "empty" | "yoloMode" | "agentMode" | "agentModeRevision" | "agentModeSupported" |
 *   "subagentConcurrencyVersion" | "workerDefaults" | "reviewDefaults" | "parkedEvent" |
 *   "pendingSettings" | "pendingSettingsAccountKey" | "queuedSettings" | "quickCreate" |
 *   "nativeThreadBlock" | "daybreakEnabled" | "accountTransfer" | "convertedFromLead" |
 *   "nativeRelease" | "activity" | "nativeStatus" | "nativeSafetyBuffering" |
 *   "nativeSafetyRetry" | "nativeTurnError" | "connectionCheck" | "readState" |
 *   "nativeLimitErrorAt" | "startAttempt" | "panelVersion" | "panelDataVersion" |
 *   "unreadCount" | "lastReadAt" | "deletedAt" | "autoWake" | "voiceState" |
 *   "nativeError" | "retryAt" | "hasUnread" | "hasQuestion" | "hasApproval" |
 *   "statusDetail" | "lastAnswer" | "lastCompletedTurn" | "nextTurnSettingsSupported" |
 *   "readStateSupported" | "pinned" | "archived" | "projectFolder" |
 *   "projectFolderRevision" | "project"
 * > & { overview?: Omit<AgentOverview, "resultFile"> | null }} TestAgent */

/** @typedef {Pick<SnapshotRoomDto, "id" | "name" | "kind" | "members" | "rootId" | "updated" | "userHidden" | "projectPath" | "radio" | "peerTeamId" | "peerTeamName" | "lastMessage">} TestRoom */
/** @typedef {Pick<SnapshotTaskDto, "id" | "turnId" | "agent" | "kind" | "status" | "created" | "finished" | "name" | "command" | "query" | "cwd" | "processId" | "durationMs" | "timeout_ms" | "interactive" | "stdinClosed" | "stdinCloseRequested" | "stdinError" | "cancelRequested" | "exitCode" | "bytes" | "log" | "outputTruncated">} TestTask */
/** @typedef {Pick<SnapshotProjectDto, "id" | "path" | "name" | "created" | "updated" | "accountKey" | "accountRevision" | "accountKeys" | "organizationRevision" | "peerTeamsRevision" | "folders" | "peerTeams">} TestProject */
/** @typedef {Pick<SnapshotComplaintDto, "id" | "leadId" | "author" | "status" | "created" | "readAt" | "recipient" | "version">} TestComplaint */
/** @typedef {{ agents: TestAgent[], rooms: TestRoom[], tasks: TestTask[], monitors: MonitorEntityDto[], complaints: TestComplaint[], projects: TestProject[], events: EventEntityDto[], peerTeams: PeerTeamEntityDto[], requests: RequestEntityDto[], rules: RuleSnapshotDto[], work: WorkSnapshotDto[], nativeNotices: RuntimeSnapshot["nativeNotices"], sidebarOrder: RuntimeSnapshot["sidebarOrder"], rateLimits: RuntimeSnapshot["rateLimits"], rateLimitsByAccount: RuntimeSnapshot["rateLimitsByAccount"], stateDir?: string }} TestRuntimeView */
/** @typedef {{ token: string, stateDir?: string, chats: SnapshotChatGroupDto[], edges: SnapshotEdgeDto[], runtime: TestRuntimeView, threads: TestAgent[] }} TestStateView */

/** @typedef {Record<string, JsonValue>} JsonObject */
/** @typedef {{ collection: string, id: string, value: JsonValue }} EntityEnvelope */

/** @type {Record<string, number>} */
const ENTITY_PAGE_SIZE = 500;

async function readJson(response, path) {
  if (!response.ok)
    throw new Error(`GET ${path} failed with HTTP ${response.status}`);
  return response.json();
}

/** @param {JsonValue} value @returns {value is JsonObject} */
function isJsonObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Read the session token and current entity rows used by browser specs.
 * The returned API is deliberately collection based; it is not a snapshot copy.
 * @param {string} origin
 * @returns {Promise<TestStateView>}
 */
export async function readTestState(origin) {
  const sessionResponse = await fetch(new URL("/api/session", origin));
  /** @type {SessionResponse} */
  const session = await readJson(sessionResponse, "/api/session");
  /** @type {Map<string, Map<string, JsonValue>>} */
  const entities = new Map();
  let after = 0;
  let fresh = true;

  for (;;) {
    const url = new URL("/api/sync/pull", origin);
    url.searchParams.set("scope", "state:entities:v1");
    url.searchParams.set("after", String(after));
    url.searchParams.set("limit", String(ENTITY_PAGE_SIZE));
    if (fresh) url.searchParams.set("fresh", "1");
    const response = await fetch(url);
    /** @type {SyncPullResponse | SyncPullResetResponse} */
    const page = await readJson(response, "/api/sync/pull");
    if (page.reset) {
      after = 0;
      fresh = true;
      continue;
    }
    fresh = false;
    for (const document of page.documents) {
      if (document._deleted) continue;
      /** @type {JsonValue} */
      const decoded = JSON.parse(document.payload);
      if (
        !isJsonObject(decoded) ||
        typeof decoded.collection !== "string" ||
        typeof decoded.id !== "string" ||
        !isJsonObject(decoded.value)
      )
        throw new Error(`Invalid entity envelope in ${document.id}`);
      /** @type {EntityEnvelope} */
      const entity = {
        collection: decoded.collection,
        id: decoded.id,
        value: decoded.value,
      };
      let collection = entities.get(entity.collection);
      if (!collection) {
        collection = new Map();
        entities.set(entity.collection, collection);
      }
      collection.set(entity.id, entity.value);
    }
    const next = page.checkpoint.seq;
    if (next <= after || next >= (page.maxSeq ?? next)) break;
    after = next;
  }

  /** @param {string} collection @returns {JsonObject[]} */
  const list = (collection) =>
    [...(entities.get(collection)?.values() ?? [])].filter(isJsonObject);
  /** @param {string} collection @param {string} id @returns {JsonObject | undefined} */
  const get = (collection, id) => {
    const value = entities.get(collection)?.get(id);
    return isJsonObject(value) ? value : undefined;
  };

  // Materialize only the collections used by browser specs. Keeping these as
  // ordinary arrays also lets fixture specs adjust the data they serve.
  const agents = /** @type {TestAgent[]} */ (list("agent"));
  const rooms = /** @type {TestRoom[]} */ (list("room"));
  const tasks = /** @type {TestTask[]} */ (list("task"));
  const monitors = /** @type {MonitorEntityDto[]} */ (list("monitor"));
  const complaints = /** @type {TestComplaint[]} */ (list("complaint"));
  const projects = /** @type {TestProject[]} */ (list("project"));
  const events = /** @type {EventEntityDto[]} */ (list("event"));
  const peerTeams = /** @type {PeerTeamEntityDto[]} */ (list("peerTeam"));
  const requests = /** @type {RequestEntityDto[]} */ (list("request"));
  const rules = /** @type {RuleSnapshotDto[]} */ (list("rule"));
  const work = /** @type {WorkSnapshotDto[]} */ (list("work"));
  const chats = /** @type {SnapshotChatGroupDto[]} */ (list("chat"));
  const edges = /** @type {SnapshotEdgeDto[]} */ (list("edge"));
  /** @type {TestRuntimeView} */
  const runtime = {
    agents,
    rooms,
    tasks,
    monitors,
    complaints,
    projects,
    events,
    peerTeams,
    requests,
    rules,
    work,
    get nativeNotices() {
      return get("workspace", "current")?.nativeNotices;
    },
    get sidebarOrder() {
      return get("workspace", "current")?.sidebarOrder;
    },
    get rateLimits() {
      return get("workspace", "current")?.rateLimits;
    },
    get rateLimitsByAccount() {
      return get("workspace", "current")?.rateLimitsByAccount;
    },
    get stateDir() {
      return get("workspace", "current")?.stateDir;
    },
  };

  return /** @type {TestStateView} */ ({
    token: session.token,
    get stateDir() {
      return get("workspace", "current")?.stateDir;
    },
    chats,
    edges,
    runtime,
    threads: agents,
  });
}

/**
 * Build an entity-pull response for HTTP stubs that previously served snapshots.
 * @param {StateSnapshot} snapshot
 * @param {number} [after]
 */
export function entityPullFixture(snapshot, after = 0) {
  const runtime = snapshot.runtime;
  const collections = [
    ["agent", runtime.agents],
    ["room", runtime.rooms],
    ["task", runtime.tasks],
    ["monitor", runtime.monitors],
    ["complaint", runtime.complaints],
    ["project", runtime.projects],
    ["event", runtime.events],
    ["peerTeam", runtime.peerTeams],
    ["request", runtime.requests],
    ["rule", runtime.rules],
    ["work", runtime.work],
    ["chat", snapshot.chats],
    ["edge", runtime.edges],
    ["workspace", [{ id: "current", ...runtime }]],
  ];
  let seq = after;
  const documents = [];
  for (const [collection, values] of collections) {
    for (const value of values ?? []) {
      if (!value || typeof value.id !== "string") continue;
      seq += 1;
      documents.push({
        id: `entity:${collection}:${value.id}`,
        seq,
        payload: JSON.stringify({ collection, id: value.id, value }),
      });
    }
  }
  return {
    documents,
    checkpoint: { seq },
    maxSeq: seq,
    initialHigh: seq,
  };
}

/**
 * Route a snapshot-shaped test fixture through the current entity sync paths.
 * @param {{ route: Function }} page
 * @param {StateSnapshot} snapshot
 * @param {string} [workspaceId]
 */
export async function stubEntityState(
  page,
  snapshot,
  workspaceId = "entity-fixture",
) {
  // SyncStore creates this value with uuid.uuid4().hex; retain a supplied
  // server-shaped id, otherwise generate the same 32-character UUID hex form.
  const identity = /^[a-f0-9]{32}$/.test(workspaceId)
    ? workspaceId
    : randomUUID().replaceAll("-", "");
  /** @type {SyncIdentityResponse} */
  const identityResponse = {
    workspaceId: identity,
    syncProtocol: 2,
    chatState: true,
  };
  // These fields and values mirror /api/sync/protocol in
  // scripts/studio_api/sync/router.py. syncProtocol on identity is the
  // identity schema version (2); the entity stream protocol is version 3.
  /** @type {SyncProtocolResponse} */
  const protocolResponse = {
    protocolVersion: 3,
    supportedVersions: [3],
    capabilities: [
      "pull",
      "stream",
      "streamChanges",
      "entityReset",
      "typedResources",
      "tokenRates",
      "unixSocket",
    ],
    scopes: ["state:entities:v1", "transcript:<agent-id>", "drafts"],
    pullEndpoint: "/api/sync/pull",
    streamEndpoint: "/api/sync/stream",
    maxEntityPage: 500,
    maxOtherPage: 100,
    maxStreamDocuments: 100,
    maxStreamBytes: 1_048_576,
  };
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: identityResponse }),
  );
  await page.route("**/api/sync/protocol", (route) =>
    route.fulfill({ json: protocolResponse }),
  );
  await page.route("**/api/sync/pull?*", (route) => {
    const after = Number(
      new URL(route.request().url()).searchParams.get("after") || 0,
    );
    /** @type {SyncPullResponse} */
    const pullResponse = {
      workspaceId: identity,
      ...entityPullFixture(snapshot, after),
    };
    return route.fulfill({
      json: pullResponse,
    });
  });
}

/**
 * Temporary path for assertions whose fields are being promoted by S2.
 * Keep every remaining GET /api/state in client specs behind this function.
 * @param {string} origin
 * @returns {Promise<StateSnapshot>}
 */
export async function readLegacySnapshotForS2Assertions(origin) {
  const response = await fetch(new URL("/api/state", origin));
  return /** @type {Promise<StateSnapshot>} */ (
    readJson(response, "/api/state")
  );
}
