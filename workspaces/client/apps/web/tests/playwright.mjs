// @ts-check

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";

const require = createRequire(
  fileURLToPath(new URL("../package.json", import.meta.url)),
);
const { test: baseTest, expect } = require("@playwright/test");
const { chromium } = require("playwright");

const generatedApiSchema = readFileSync(
  new URL("../src/generated/apiSchema.ts", import.meta.url),
  "utf8",
);

export const API_SCHEMA_HASH_HEADER = readGeneratedString(
  "API_SCHEMA_HASH_HEADER",
);
export const API_SCHEMA_HASH_PARAM = readGeneratedString(
  "API_SCHEMA_HASH_PARAM",
);
export const legacySnapshotRoute = /\/api\/state(?:\?.*)?$/;

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
  // @ts-expect-error This module adds a private fixture to the shared test type.
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

  const child = spawn(command, args, {
    ...options,
    env: {
      ...process.env,
      ...options?.env,
      XDG_CACHE_HOME: join(process.env.TMPDIR || tmpdir(), "fixture-cache"),
    },
  });
  scope.children.add(child);
  child.on("error", (error) => {
    if (/** @type {NodeJS.ErrnoException} */ (error).code !== "ESRCH")
      scope.errors.push(error);
  });
  child.once("close", () => scope.children.delete(child));
  return child;
}

export const browserExecutablePath =
  process.env.CHROME_BIN?.trim() || chromium.executablePath();
/** @typedef {import("../src/generated/api").components["schemas"]["JsonValue"]} JsonValue */
/** @typedef {import("../src/generated/api").components["schemas"]["SessionResponse"]} SessionResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncPullResponse"]} SyncPullResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncPullResetResponse"]} SyncPullResetResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncEntity"]} SyncEntity */
/** @typedef {import("../src/generated/api").components["schemas"]["TranscriptPageResponse"]} TranscriptPageResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncIdentityResponse"]} SyncIdentityResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncProtocolResponse"]} SyncProtocolResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceChangeEvent"]} ResourceChangeEvent */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceHeartbeatEvent"]} ResourceHeartbeatEvent */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceTokenRatesEvent"]} ResourceTokenRatesEvent */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceRef"]} ResourceRef */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceNotifyRequest"]} ResourceNotifyRequest */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceNotifyAck"]} ResourceNotifyAck */
/** @typedef {import("../src/generated/api").components["schemas"]["AgentEntityDto"]} AgentEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["ChatEntityDto"]} ChatEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["ComplaintEntityDto"]} ComplaintEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["EdgeEntityDto"]} EdgeEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["ProjectEntityDto"]} ProjectEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["RoomEntityDto"]} RoomEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["RuleEntityDto"]} RuleEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["TaskEntityDto"]} TaskEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["WorkspaceEntityDto"]} WorkspaceEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["WorkEntityDto"]} WorkEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["EventEntityDto"]} EventEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["MonitorEntityDto"]} MonitorEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["PeerTeamEntityDto"]} PeerTeamEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["RequestEntityDto"]} RequestEntityDto */

/**
 * Keep this view limited to fields in AgentEntityDto.
 * @typedef {Pick<AgentEntityDto,
 *   "id" | "name" | "manualName" | "status" | "source" | "kind" |
 *   "parentId" | "rootId" | "threadId" | "orchestratorId" | "orchestratorName" |
 *   "isLead" | "role" | "sharedRoomId" | "model" | "provider" | "effort" |
 *   "fastMode" | "concurrency" | "accountKey" | "cwd" | "worktree" |
 *   "worktreePreparation" | "imageWorkspace" | "imageWorkspaceReady" |
 *   "imageWorkspacePhase" | "imageWorkspaceError" | "imageWorkspaceRepo" |
 *   "imageWorkspaceBaseRepo" | "imageWorkspaceSubpath" | "imageWorkspaceBaseRef" |
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
 *   "projectFolderRevision" | "project" | "projectId" | "projectServerId" | "serverId"
 * >} TestAgent */

/** @typedef {Pick<RoomEntityDto, "id" | "name" | "kind" | "members" | "rootId" | "updated" | "userHidden" | "projectPath" | "radio" | "peerTeamId" | "peerTeamName" | "lastMessage" | "peerLabel" | "localMembers">} TestRoom */
/** @typedef {Pick<TaskEntityDto, "id" | "turnId" | "agent" | "kind" | "status" | "created" | "finished" | "name" | "command" | "query" | "cwd" | "processId" | "durationMs" | "timeout_ms" | "interactive" | "stdinClosed" | "stdinCloseRequested" | "stdinError" | "cancelRequested" | "exitCode" | "bytes" | "log" | "outputTruncated">} TestTask */
/** @typedef {Pick<ProjectEntityDto, "homeServerId" | "locations" | "locationsRevision" | "projectAliases" | "id" | "path" | "name" | "created" | "updated" | "accountKey" | "accountRevision" | "accountKeys" | "organizationRevision" | "peerTeamsRevision" | "folders" | "peerTeams" | "workerBaseRef" | "workerBaseRevision" | "workerEnvironment" | "workerEnvironmentRevision">} TestProject */
/** @typedef {Pick<ComplaintEntityDto, "id" | "leadId" | "author" | "authorName" | "leadName" | "title" | "status" | "needsUserResponse" | "created" | "readAt" | "recipient" | "version">} TestComplaint */
/** @typedef {{ agents: TestAgent[], rooms: TestRoom[], tasks: TestTask[], monitors: MonitorEntityDto[], complaints: TestComplaint[], projects: TestProject[], events: EventEntityDto[], peerTeams: PeerTeamEntityDto[], requests: RequestEntityDto[], rules: RuleEntityDto[], work: WorkEntityDto[], connected?: WorkspaceEntityDto["connected"], peerTeamsVersion?: WorkspaceEntityDto["peerTeamsVersion"], projectOrganizationVersion?: WorkspaceEntityDto["projectOrganizationVersion"], tasksHistoryLimit?: WorkspaceEntityDto["tasksHistoryLimit"], nativeNotices: WorkspaceEntityDto["nativeNotices"], sidebarOrder: WorkspaceEntityDto["sidebarOrder"], rateLimits: WorkspaceEntityDto["rateLimits"], rateLimitsByAccount: WorkspaceEntityDto["rateLimitsByAccount"], stateDir?: string }} TestRuntimeView */
/** @typedef {{ token: string, stateDir?: string, chats: ChatEntityDto[], edges: EdgeEntityDto[], runtime: TestRuntimeView, threads: TestAgent[] }} TestStateView */
/** @typedef {Omit<TestStateView, "token" | "threads">} EntityFixtureState */

/** @typedef {Record<string, JsonValue>} JsonObject */
/** @typedef {{ collection: string, id: string, value: JsonValue }} EntityEnvelope */

/** @typedef {{ scope?: string, after?: number, limit?: number, fresh?: boolean, initialHigh?: number, reset?: boolean, floor?: number, maxSeq?: number, generation?: number, priorityId?: string, tombstones?: SyncEntity[], transcript?: TranscriptPageResponse, drafts?: SyncEntity[], documents?: SyncEntity[] }} EntityPullFixtureOptions */

/** @type {number} */
const ENTITY_PAGE_SIZE = 500;
/** @type {ResourceChangeEvent["protocol"]} */
const RESOURCE_STREAM_PROTOCOL = 3;

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
  const sessionUrl = new URL("/api/session", origin);
  const firstPullUrl = new URL("/api/sync/pull", origin);
  firstPullUrl.searchParams.set("scope", "state:entities:v1");
  firstPullUrl.searchParams.set("after", "0");
  firstPullUrl.searchParams.set("limit", String(ENTITY_PAGE_SIZE));
  firstPullUrl.searchParams.set("fresh", "1");
  const [sessionResponse, firstPullResponse] = await Promise.all([
    fetch(sessionUrl),
    fetch(firstPullUrl),
  ]);
  /** @type {SessionResponse} */
  const session = await readJson(sessionResponse, "/api/session");
  /** @type {Map<string, Map<string, JsonValue>>} */
  const entities = new Map();
  let after = 0;
  let fresh = false;
  let nextResponse = firstPullResponse;

  for (;;) {
    /** @type {SyncPullResponse | SyncPullResetResponse} */
    const page = await readJson(nextResponse, "/api/sync/pull");
    if (page.reset === true) {
      after = 0;
      fresh = true;
    } else {
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
    const url = new URL("/api/sync/pull", origin);
    url.searchParams.set("scope", "state:entities:v1");
    url.searchParams.set("after", String(after));
    url.searchParams.set("limit", String(ENTITY_PAGE_SIZE));
    if (fresh) url.searchParams.set("fresh", "1");
    nextResponse = await fetch(url);
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
  const rules = /** @type {RuleEntityDto[]} */ (list("rule"));
  const work = /** @type {WorkEntityDto[]} */ (list("work"));
  const chats = /** @type {ChatEntityDto[]} */ (list("chat"));
  const edges = /** @type {EdgeEntityDto[]} */ (list("edge"));
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
    connected: /** @type {WorkspaceEntityDto["connected"]} */ (
      get("workspace", "current")?.connected
    ),
    peerTeamsVersion: /** @type {WorkspaceEntityDto["peerTeamsVersion"]} */ (
      get("workspace", "current")?.peerTeamsVersion
    ),
    projectOrganizationVersion:
      /** @type {WorkspaceEntityDto["projectOrganizationVersion"]} */ (
        get("workspace", "current")?.projectOrganizationVersion
      ),
    tasksHistoryLimit: /** @type {WorkspaceEntityDto["tasksHistoryLimit"]} */ (
      get("workspace", "current")?.tasksHistoryLimit
    ),
    get nativeNotices() {
      return /** @type {WorkspaceEntityDto["nativeNotices"]} */ (
        get("workspace", "current")?.nativeNotices
      );
    },
    get sidebarOrder() {
      return /** @type {WorkspaceEntityDto["sidebarOrder"]} */ (
        get("workspace", "current")?.sidebarOrder
      );
    },
    get rateLimits() {
      return /** @type {WorkspaceEntityDto["rateLimits"]} */ (
        get("workspace", "current")?.rateLimits
      );
    },
    get rateLimitsByAccount() {
      return /** @type {WorkspaceEntityDto["rateLimitsByAccount"]} */ (
        get("workspace", "current")?.rateLimitsByAccount
      );
    },
    get stateDir() {
      return /** @type {string | undefined} */ (
        get("workspace", "current")?.stateDir
      );
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

/** @type {(keyof WorkspaceEntityDto)[]} */
const entityWorkspaceKeys = [
  "connected",
  "nativeNotices",
  "peerTeamsVersion",
  "projectOrganizationVersion",
  "rateLimits",
  "rateLimitsByAccount",
  "sidebarOrder",
  "stateDir",
  "tasksHistoryLimit",
];

/** @type {Set<string>} */
const snapshotOnlyAgentKeys = new Set([
  "activeTools",
  "compactionsObservedOnly",
  "complaintsPresented",
  "cyberAccessProgram",
  "events",
  "executionSettingsAccountKey",
  "maxAgents",
  "maxAgentsExplicit",
  "nativeEffort",
  "nativeNameFailure",
  "nativeToolCatalog",
  "needsTitle",
  "prepareAttempt",
  "preparedContext",
  "profileId",
  "profileInstructions",
  "tokenBudget",
  "turnEpoch",
  "usageResumeEnabled",
  "wave",
  "workerBaseBehindMain",
  "workerBaseCommit",
  "workerBaseMainRef",
  "workerBaseRef",
  "worktreeReady",
  "worktreeWarning",
]);

/** @template {object} T @param {T} value @param {Iterable<keyof T>} keys @returns {JsonObject} */
function pickJsonKeys(value, keys) {
  const source = /** @type {Record<string, JsonValue>} */ (value);
  /** @type {JsonObject} */
  const picked = {};
  for (const key of keys) {
    const jsonKey = String(key);
    if (Object.hasOwn(source, jsonKey)) picked[jsonKey] = source[jsonKey];
  }
  return picked;
}

/** @param {SyncEntity} document @returns {SyncEntity} */
function generatedSyncEntity(document) {
  return document;
}

/** @param {SyncPullResponse | SyncPullResetResponse} response @returns {SyncPullResponse | SyncPullResetResponse} */
function generatedSyncPullResponse(response) {
  return response;
}

/**
 * Build entity values from a fixture, omitting fields that the entity contract
 * does not expose.
 * @param {EntityFixtureState} snapshot
 * @returns {{ collection: string, id: string, value: JsonValue }[]}
 */
function entityValuesFromSnapshot(snapshot) {
  const runtime = snapshot.runtime;
  /** @type {{ collection: string, id: string, value: JsonValue }[]} */
  const values = [];
  /** @param {string} collection @param {Array<{id?: string}> | null | undefined} rows @param {(row: object) => JsonValue} [project] */
  const append = (
    collection,
    rows,
    project = (row) => /** @type {JsonValue} */ (row),
  ) => {
    for (const row of rows ?? []) {
      if (typeof row.id !== "string") continue;
      values.push({ collection, id: row.id, value: project(row) });
    }
  };
  append("project", runtime.projects, (row) =>
    pickJsonKeys(row, [
      "homeServerId",
      "locations",
      "locationsRevision",
      "projectAliases",
      "id",
      "path",
      "name",
      "created",
      "updated",
      "accountKey",
      "accountRevision",
      "accountKeys",
      "organizationRevision",
      "peerTeamsRevision",
      "folders",
      "peerTeams",
      "workerBaseRef",
      "workerBaseRevision",
      "workerEnvironment",
      "workerEnvironmentRevision",
    ]),
  );
  append("request", runtime.requests);
  append("event", runtime.events, (row) =>
    pickJsonKeys(row, ["id", "agent", "kind", "status", "created", "error"]),
  );
  append("room", runtime.rooms, (row) =>
    pickJsonKeys(row, [
      "id",
      "name",
      "kind",
      "members",
      "rootId",
      "updated",
      "userHidden",
      "projectPath",
      "radio",
      "peerTeamId",
      "peerTeamName",
      "lastMessage",
      "peerLabel",
      "localMembers",
    ]),
  );
  append("agent", runtime.agents, (row) => {
    const entity = /** @type {Record<string, JsonValue>} */ ({ ...row });
    for (const key of snapshotOnlyAgentKeys) delete entity[key];
    const agentNestedAllowLists = {
      activity: ["phase", "at", "tools"],
      nativeStatus: ["phase", "error", "message", "turnId", "at"],
      nativeSafetyBuffering: [
        "turnId",
        "threadId",
        "accountKey",
        "connectionId",
        "at",
        "dismissed",
        "responseStarted",
        "showBufferingUi",
        "fasterModel",
      ],
      nativeSafetyRetry: [
        "id",
        "stage",
        "model",
        "turnId",
        "created",
        "updated",
        "epoch",
        "accountKey",
        "error",
        "newThreadId",
        "acceptedTurnId",
        "requestId",
        "rpcMethod",
      ],
      nativeTurnError: ["turnId", "error"],
      nativeThreadBlock: ["threadId", "error"],
      connectionCheck: [
        "epoch",
        "accountKey",
        "threadId",
        "turnId",
        "at",
        "previousError",
        "nativeState",
        "restartTurnStatus",
        "readError",
      ],
      readState: ["threadId", "turnId", "read", "revision"],
      startAttempt: ["prepareError", "responseError", "retiredEvents"],
    };
    for (const [key, allowed] of Object.entries(agentNestedAllowLists)) {
      if (isJsonObject(entity[key]))
        entity[key] = pickJsonKeys(entity[key], allowed);
    }
    if (isJsonObject(entity.nativeRelease))
      entity.nativeRelease = pickJsonKeys(entity.nativeRelease, [
        "phase",
        "resetPending",
      ]);
    return entity;
  });
  append("task", runtime.tasks);
  append("monitor", runtime.monitors);
  append("complaint", runtime.complaints, (row) => {
    const entity = /** @type {Record<string, JsonValue>} */ ({ ...row });
    delete entity.needsResponse;
    return entity;
  });
  append("peerTeam", runtime.peerTeams);
  append("rule", runtime.rules);
  append("work", runtime.work);
  append("chat", snapshot.chats);
  append("edge", snapshot.edges);
  /** @type {Record<string, JsonValue>} */
  const workspaceSource = { ...runtime, stateDir: snapshot.stateDir };
  values.push({
    collection: "workspace",
    id: "current",
    value: pickJsonKeys(workspaceSource, entityWorkspaceKeys),
  });
  return values;
}

/** @param {string} [workspaceId] @returns {SyncIdentityResponse} */
export function syncIdentityFixture(
  workspaceId = randomUUID().replaceAll("-", ""),
) {
  if (!/^[a-f0-9]{32}$/.test(workspaceId))
    throw new Error(
      "Sync fixture workspaceId must match the server's 32-hex identity",
    );
  return { workspaceId, syncProtocol: 2 };
}

/** @param {{ unixSocket?: boolean }} [options] @returns {SyncProtocolResponse} */
export function syncProtocolFixture({ unixSocket = false } = {}) {
  return {
    protocolVersion: 3,
    supportedVersions: [3],
    capabilities: [
      "pull",
      "stream",
      "streamChanges",
      "entityReset",
      "typedResources",
      "tokenRates",
      ...(unixSocket ? ["unixSocket"] : []),
    ],
    scopes: ["state:entities:v1", "transcript:<agent-id>", "drafts"],
    pullEndpoint: "/api/sync/pull",
    streamEndpoint: "/api/sync/stream",
    maxEntityPage: 500,
    maxOtherPage: 100,
    maxStreamDocuments: 100,
    maxStreamBytes: 1_048_576,
  };
}

/** @param {string} origin @returns {Promise<{ identity: SyncIdentityResponse, protocol: SyncProtocolResponse }>} */
export async function readFixtureSyncContract(origin) {
  const [identityResponse, protocolResponse] = await Promise.all([
    fetch(new URL("/api/sync/identity", origin)),
    fetch(new URL("/api/sync/protocol", origin)),
  ]);
  const identity = /** @type {SyncIdentityResponse} */ (
    await readJson(identityResponse, "/api/sync/identity")
  );
  const protocol = /** @type {SyncProtocolResponse} */ (
    await readJson(protocolResponse, "/api/sync/protocol")
  );
  if (
    !/^[a-f0-9]{32}$/.test(identity.workspaceId) ||
    identity.syncProtocol !== 2 ||
    protocol.protocolVersion !== 3
  )
    throw new Error("Fixture server returned an invalid sync contract");
  return { identity, protocol };
}

/** @typedef {{ documents: Map<string, SyncEntity>, maxSeq: number }} MutableEntityPullState */

/** @type {WeakMap<EntityFixtureState, MutableEntityPullState>} */
const nodeEntityPullStates = new WeakMap();

/** @returns {MutableEntityPullState} */
function createMutableEntityPullState() {
  return { documents: new Map(), maxSeq: 0 };
}

/**
 * Version changed entities and tombstones so clients that have advanced their
 * cursor can observe fixture state changes on a later pull.
 * @param {MutableEntityPullState} pullState
 * @param {EntityFixtureState} snapshot
 */
function reconcileMutableEntityPullState(pullState, snapshot) {
  const nextById = new Map(
    entityValuesFromSnapshot(snapshot).map((entity) => [
      `entity:${entity.collection}:${entity.id}`,
      entity,
    ]),
  );
  for (const [id, previous] of pullState.documents) {
    if (previous._deleted) continue;
    const next = nextById.get(id);
    if (!next) {
      pullState.documents.set(
        id,
        generatedSyncEntity({
          ...previous,
          seq: ++pullState.maxSeq,
          _deleted: true,
        }),
      );
      continue;
    }
    const payload = JSON.stringify(next);
    if (payload !== previous.payload)
      pullState.documents.set(
        id,
        generatedSyncEntity({
          ...previous,
          seq: ++pullState.maxSeq,
          payload,
          _deleted: false,
        }),
      );
  }
  for (const [id, entity] of nextById) {
    const previous = pullState.documents.get(id);
    if (!previous || previous._deleted)
      pullState.documents.set(
        id,
        generatedSyncEntity({
          id,
          seq: ++pullState.maxSeq,
          payload: JSON.stringify(entity),
          _deleted: false,
        }),
      );
  }
}

/**
 * Commit a fixture state change and return the entity documents its mutation
 * response should carry. This mirrors the server's `_syncEntities` envelope.
 * @param {EntityFixtureState} snapshot
 * @returns {SyncEntity[]}
 */
export function updateEntitySyncFixture(snapshot) {
  let pullState = nodeEntityPullStates.get(snapshot);
  if (!pullState) {
    pullState = createMutableEntityPullState();
    nodeEntityPullStates.set(snapshot, pullState);
  }
  const previousMaxSeq = pullState.maxSeq;
  reconcileMutableEntityPullState(pullState, snapshot);
  return [...pullState.documents.values()].filter(
    (document) => document.seq > previousMaxSeq,
  );
}

/**
 * Serve the same sync routes from a small Node HTTP fixture backend.
 * @param {import("node:http").IncomingMessage} request
 * @param {import("node:http").ServerResponse} response
 * @param {{ snapshot: EntityFixtureState, workspaceId: string, unixSocket?: boolean, initialResources?: ResourceRef[], onPull?: () => void, onStreamReady?: (notify: (resources: ResourceRef[]) => void) => void }} fixture
 * @returns {boolean} whether this was an /api/sync route
 */
export function handleEntitySyncFixtureRequest(request, response, fixture) {
  const url = new URL(request.url || "/", "http://fixture.invalid");
  if (!url.pathname.startsWith("/api/sync/")) return false;
  const identity = syncIdentityFixture(fixture.workspaceId);
  const json = (value) => {
    response.setHeader("Content-Type", "application/json");
    response.end(JSON.stringify(value));
  };
  if (url.pathname === "/api/sync/identity") {
    response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
    json(identity);
    return true;
  }
  if (url.pathname === "/api/sync/protocol") {
    json(syncProtocolFixture({ unixSocket: fixture.unixSocket }));
    return true;
  }
  if (url.pathname === "/api/sync/pull") {
    fixture.onPull?.();
    const validation = validateSyncPullRequest(url);
    if (validation.ok === false) {
      response.statusCode = validation.status;
      json(validation.body);
      return true;
    }
    /** @type {EntityPullFixtureOptions} */
    let pullOptions = validation.params;
    if (validation.params.scope === "state:entities:v1") {
      let pullState = nodeEntityPullStates.get(fixture.snapshot);
      if (!pullState) {
        pullState = createMutableEntityPullState();
        nodeEntityPullStates.set(fixture.snapshot, pullState);
      }
      reconcileMutableEntityPullState(pullState, fixture.snapshot);
      pullOptions = {
        ...validation.params,
        documents: [...pullState.documents.values()],
        maxSeq: pullState.maxSeq,
      };
    }
    const pullResponse = generatedSyncPullResponse({
      workspaceId: identity.workspaceId,
      ...entityPullFixture(fixture.snapshot, pullOptions),
    });
    json(pullResponse);
    return true;
  }
  if (url.pathname === "/api/sync/stream") {
    const epoch = randomUUID().replaceAll("-", "");
    let revision = 0;
    let eventId = 0;
    response.writeHead(200, {
      "Cache-Control": "no-cache, no-transform",
      Connection: "keep-alive",
      "Content-Type": "text/event-stream; charset=utf-8",
      [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      "X-Accel-Buffering": "no",
    });
    response.write(apiSchemaHandshakeSse());
    /** @param {string} name @param {ResourceChangeEvent | ResourceHeartbeatEvent | ResourceTokenRatesEvent} value */
    const writeFrame = (name, value) => {
      eventId += 1;
      response.write(
        `event: ${name}\ndata: ${JSON.stringify(value)}\nid: ${eventId}\n\n`,
      );
    };
    /** @type {ResourceChangeEvent} */
    const initial = {
      epoch,
      protocol: RESOURCE_STREAM_PROTOCOL,
      reason: "initial",
      resources: fixture.initialResources ?? [{ kind: "state" }],
      // No per-resource versions: the page must always pull after a fixture frame.
      resourceVersions: [],
      revision: ++revision,
      workspaceId: identity.workspaceId,
    };
    writeFrame("resources", initial);
    /** @type {ResourceTokenRatesEvent} */
    const rates = {
      epoch,
      protocol: RESOURCE_STREAM_PROTOCOL,
      rates: {},
      revision,
      teams: {},
      workspaceId: identity.workspaceId,
    };
    writeFrame("token-rates", rates);
    const heartbeat = () => {
      if (response.destroyed) return;
      /** @type {ResourceHeartbeatEvent} */
      const event = {
        epoch,
        protocol: RESOURCE_STREAM_PROTOCOL,
        revision: ++revision,
        workspaceId: identity.workspaceId,
      };
      writeFrame("heartbeat", event);
    };
    const heartbeatTimer = setInterval(heartbeat, 5000);
    heartbeatTimer.unref?.();
    response.once("close", () => clearInterval(heartbeatTimer));
    fixture.onStreamReady?.((resources) => {
      if (response.destroyed) return;
      /** @type {ResourceChangeEvent} */
      const changed = {
        epoch,
        protocol: RESOURCE_STREAM_PROTOCOL,
        reason: "change",
        resources,
        resourceVersions: [],
        revision: ++revision,
        workspaceId: identity.workspaceId,
      };
      writeFrame("resources", changed);
    });
    return true;
  }
  response.statusCode = 404;
  json({ error: "Not found" });
  return true;
}

/**
 * Build a typed entity-pull fixture with the protocol's page, scope, tombstone,
 * reset, priority and checkpoint semantics.
 * @param {EntityFixtureState} snapshot
 * @param {number | EntityPullFixtureOptions} [optionsOrAfter]
 * @returns {Omit<SyncPullResponse, "workspaceId"> | Omit<SyncPullResetResponse, "workspaceId">}
 */
export function entityPullFixture(snapshot, optionsOrAfter = {}) {
  const options =
    typeof optionsOrAfter === "number"
      ? { after: optionsOrAfter }
      : optionsOrAfter;
  const scope = options.scope ?? "state:entities:v1";
  const after = Math.max(0, options.after ?? 0);
  const generation = options.generation ?? 1;
  if (scope === "drafts") {
    const drafts = options.drafts ?? [];
    const high = drafts.at(-1)?.seq ?? 0;
    const limit = Math.min(100, Math.max(1, options.limit ?? 100));
    const page = drafts
      .filter((document) => document.seq > after)
      .slice(0, limit);
    return {
      generation,
      documents: page,
      checkpoint: {
        seq: page.length === limit ? page.at(-1).seq : high,
      },
    };
  }
  if (scope.startsWith("transcript:")) {
    const high = 1;
    const limit = Math.min(100, Math.max(1, options.limit ?? 100));
    /** @type {TranscriptPageResponse} */
    const transcript = options.transcript ?? {
      items: [],
      truncated: false,
      agent: null,
      historyVersion: "0",
      nextAfterCursor: null,
      nextCursor: null,
      tail: null,
      unavailable: null,
    };
    const doc = generatedSyncEntity({
      id: scope,
      seq: high,
      payload: JSON.stringify(transcript),
      _deleted: false,
    });
    return {
      generation,
      documents: after < high ? [doc].slice(0, limit) : [],
      checkpoint: { seq: after < high ? high : after },
    };
  }
  if (scope !== "state:entities:v1")
    throw new Error(`Unsupported fixture pull scope: ${scope}`);

  const limit = Math.min(500, Math.max(1, options.limit ?? 500));
  const values = entityValuesFromSnapshot(snapshot);
  const baseDocuments = values.map((entity, index) =>
    generatedSyncEntity({
      id: `entity:${entity.collection}:${entity.id}`,
      seq: index + 1,
      payload: JSON.stringify(entity),
      _deleted: false,
    }),
  );
  const tombstones = options.tombstones ?? [];
  const documents = [
    ...(options.documents ?? baseDocuments),
    ...tombstones,
  ].sort((left, right) => left.seq - right.seq);
  const high = options.maxSeq ?? documents.at(-1)?.seq ?? 0;
  const floor = options.floor ?? 0;
  const fresh = options.fresh === true;
  const initialHigh = fresh
    ? options.initialHigh && options.initialHigh > 0
      ? Math.min(high, options.initialHigh)
      : high
    : 0;
  if (options.reset && !fresh && after > 0 && after < floor)
    return { generation, reset: true, floor, maxSeq: high };

  let eligible = documents.filter(
    (document) =>
      document.seq > after &&
      (!document._deleted || document.seq > floor) &&
      (!fresh || !document._deleted || document.seq > initialHigh),
  );
  if (after === 0 && fresh && options.priorityId) {
    eligible = eligible.sort((left, right) => {
      const priority = `entity:agent:${options.priorityId}`;
      return Number(right.id === priority) - Number(left.id === priority);
    });
  }
  const page = eligible.slice(0, limit);
  const checkpoint = page.length === limit ? page.at(-1).seq : high;
  return {
    generation,
    documents: page,
    checkpoint: { seq: checkpoint },
    maxSeq: high,
    initialHigh,
  };
}

/**
 * Parse /api/sync/pull query semantics shared by the browser and Node fixtures.
 * The real API accepts negative integer cursors (the store clamps them to zero)
 * and returns 400 for malformed integers and unknown scopes.
 * @param {string | URL} requestUrl
 * @returns {{ ok: true, params: Required<Pick<EntityPullFixtureOptions, "scope" | "after" | "limit" | "fresh" | "initialHigh" | "reset">> & Pick<EntityPullFixtureOptions, "priorityId"> } | { ok: false, status: number, body: JsonObject }}
 */
export function validateSyncPullRequest(requestUrl) {
  const url = new URL(requestUrl, "http://fixture.invalid");
  const query = url.searchParams;
  const first = (key) => query.getAll(key).find((value) => value !== "");
  const scope = first("scope") ?? "state:entities:v1";
  /** @type {Record<string, number>} */
  const numbers = {};
  /** @type {Array<[keyof typeof numbers, number]>} */
  const defaults = [
    ["after", 0],
    ["limit", 500],
    ["initialHigh", 0],
  ];
  for (const [key, fallback] of defaults) {
    const raw = first(key);
    if (raw === undefined) {
      numbers[key] = fallback;
      continue;
    }
    if (!/^[+-]?\d+$/.test(raw))
      return {
        ok: false,
        status: 400,
        body: {
          error: "Invalid request",
          details: [
            {
              location: ["query", key],
              message:
                "Input should be a valid integer, unable to parse string as an integer",
            },
          ],
        },
      };
    numbers[key] = Number(raw);
  }
  const supportedScope =
    scope === "state:entities:v1" ||
    scope === "drafts" ||
    (scope.startsWith("transcript:") && scope.length < 300);
  if (!supportedScope)
    return {
      ok: false,
      status: 400,
      body: { error: "Invalid sync scope" },
    };
  return {
    ok: true,
    params: {
      scope,
      after: numbers.after,
      limit: numbers.limit,
      fresh: first("fresh") === "1",
      initialHigh: numbers.initialHigh,
      reset: first("reset") === "1",
      priorityId: first("priorityId"),
    },
  };
}

/**
 * Convert an incoming fixture request URL to the shared pull fixture options.
 * @param {EntityFixtureState} snapshot
 * @param {string | URL} requestUrl
 * @param {EntityPullFixtureOptions} [overrides]
 * @returns {Omit<SyncPullResponse, "workspaceId"> | Omit<SyncPullResetResponse, "workspaceId">}
 */
export function entityPullFixtureForRequest(
  snapshot,
  requestUrl,
  overrides = {},
) {
  const validation = validateSyncPullRequest(requestUrl);
  if (validation.ok === false) {
    const error = new Error(String(validation.body.error));
    Object.assign(error, {
      status: validation.status,
      body: validation.body,
    });
    throw error;
  }
  return entityPullFixture(snapshot, { ...validation.params, ...overrides });
}

/**
 * Route a snapshot-shaped test fixture through the current entity sync paths.
 * @param {{ route: Function }} page
 * @param {EntityFixtureState} snapshot
 * @param {{ identity: SyncIdentityResponse, protocol: SyncProtocolResponse }} contract
 * @returns {Promise<{ update: (nextSnapshot: EntityFixtureState, options?: { origin: string, token: string, resources?: ResourceRef[] }) => Promise<ResourceNotifyAck | undefined> }>}
 */
export async function stubEntityState(page, snapshot, contract) {
  const { identity: identityResponse, protocol: protocolResponse } = contract;
  if (
    !/^[a-f0-9]{32}$/.test(identityResponse.workspaceId) ||
    identityResponse.syncProtocol !== 2 ||
    protocolResponse.protocolVersion !== 3
  )
    throw new Error(
      "stubEntityState requires the fixture backend's generated sync contract",
    );
  const identity = identityResponse.workspaceId;
  let generation = 1;
  /** @type {EntityFixtureState} */
  let currentSnapshot = snapshot;
  const pullState = createMutableEntityPullState();
  reconcileMutableEntityPullState(pullState, currentSnapshot);
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({
      json: identityResponse,
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
    }),
  );
  await page.route("**/api/sync/protocol", (route) =>
    route.fulfill({ json: protocolResponse }),
  );
  // Keep a real backend stream open: route.fulfill closes SSE immediately and
  // EventSource then reconnects continuously. The pull route below remains the
  // deterministic entity fixture; the live stream exercises real frame shape.
  await page.route("**/api/sync/stream?*", (route) => route.continue());
  await page.route("**/api/sync/pull**", (route) => {
    const requestUrl = route.request().url();
    const validation = validateSyncPullRequest(requestUrl);
    if (validation.ok === false)
      return route.fulfill({
        status: validation.status,
        json: validation.body,
      });
    reconcileMutableEntityPullState(pullState, currentSnapshot);
    const pageData = entityPullFixture(currentSnapshot, {
      ...validation.params,
      generation,
      documents: [...pullState.documents.values()],
      maxSeq: pullState.maxSeq,
    });
    return route.fulfill({
      json: generatedSyncPullResponse({
        workspaceId: identity,
        ...pageData,
      }),
    });
  });

  /**
   * Update changed entity documents with monotonic sequences and tombstone
   * removed entities. If origin/token are provided, publish the state
   * invalidation as a stand-in for the missing base server commit notification.
   * @param {EntityFixtureState} nextSnapshot
   * @param {{ origin: string, token: string, resources?: ResourceRef[] }} [publish]
   * @returns {Promise<ResourceNotifyAck | undefined>}
   */
  const update = async (nextSnapshot, publish) => {
    currentSnapshot = nextSnapshot;
    reconcileMutableEntityPullState(pullState, currentSnapshot);
    generation++;
    if (!publish) return undefined;
    const body = /** @type {ResourceNotifyRequest} */ ({
      requestId: randomUUID(),
      resources: publish.resources ?? [{ kind: "state" }],
    });
    const response = await fetch(new URL("/api/sync/notify", publish.origin), {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": publish.token,
        [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      },
      body: JSON.stringify(body),
    });
    return /** @type {ResourceNotifyAck} */ (
      await readJson(response, "/api/sync/notify")
    );
  };
  return { update };
}
