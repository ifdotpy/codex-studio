#!/usr/bin/env node
// Scenario smoke of the Codex Studio installed application.
//
// It checks the main user scenarios against the live backend: the app and
// backend answer, a test chat gets a turn from each provider with a working
// login, a message is delivered once, a monitor runs and reports its exit, a
// sign-in card is created and declined, and the client transcript state is
// current. It reads the owner's data only. It writes only its own test chats
// ("Studio smoke <run>"), which it archives at the end, its own project entry,
// which it removes, and its own temporary folder, which it deletes by exact
// path and run marker. docs/testing.md, "Installed scenario smoke", owns the
// contract.

import { createHash, randomBytes, randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import {
  compareEvidence,
  readEvidence,
  pricingModels,
  isRecord,
  usageTable,
  nativeEvidence,
  recordedUsage,
  sumTokens,
  priceResponses,
  accountingEvidence,
  scenarioEvidence,
  publicationReady,
  bridgeProcessMatches,
  validateSteer,
  atomicWriteJson,
  preserveProvenance,
  verifiedPermissions,
  NOTES_FILE,
  NOTES_MARKER,
  directoryIdentity,
  processStartIdentity,
  sqliteEvidence,
} from "./usage.ts";

import assert from "node:assert/strict";
import type {
  Schema,
  Provider,
  Transcript,
  Analytics,
  Cost,
  DirectoryIdentity,
  Timing,
  ProcessRow,
  Evidence,
  BridgeObservation,
  Steer,
  Optional,
} from "./usage.ts";

import type { PricingArtifact } from "../../../client/apps/web/src/generated/pricing.ts";
import type { RendererManifest } from "../../../client/apps/web/scripts/build-manifests.ts";

type Options = {
  app: string;
  url: string;
  model: Record<string, string>;
  usage?: boolean;
  providers?: string[] | undefined;
  "sqlite-ro-agent"?: string | undefined;
};
export type ReportRow = {
  name: string;
  status: "pass" | "FAIL" | "skip";
  detail: string;
  ms: number;
};
type Chat = {
  id: string;
  account?: string | undefined;
  model?: string | undefined;
  detail?: string | undefined;
};
type Probe<T> = { done?: unknown; value?: T; note?: string | undefined };

type LiveEntity = Record<string, unknown> &
  Optional<Schema["AgentDetailEntityDto"]> &
  Optional<Pick<Schema["AgentSummaryEntityDto"], "turnId">> &
  Cost &
  Optional<Pick<Schema["MonitorEntityDto"], "agent" | "exitCode">> & {
    status?:
      | Schema["MonitorEntityDto"]["status"]
      | Schema["RequestEntityDto"]["status"]
      | Schema["EventEntityDto"]["status"]
      | Schema["MessageReceiptEntityDto"]["status"];
  };
export type OwnedEntities = {
  analytics: Analytics;
  agent: Optional<
    Pick<
      Schema["AnalyticsAgentRecord"],
      "id" | "accountKey" | "cwd" | "model" | "name" | "provider" | "threadId"
    >
  > &
    Optional<Pick<Schema["AnalyticsAgentRecord"], "tokenUsageAccounting">> &
    Optional<Schema["AgentDetailEntityDto"]>;
  sessionCost: Cost;
  publication: {
    finalTurnId: string | null | undefined;
    currentCost: Cost;
    capturedAt: string;
  };
};

const DEFAULT_APP =
  process.platform === "linux"
    ? path.join(os.homedir(), ".local/share/codex-studio/server/current")
    : "/Applications/Codex Studio.app";

export function installationPaths(
  app: string,
  platform: NodeJS.Platform = process.platform,
) {
  const workspace =
    platform === "linux" ? app : path.join(app, "Contents/Resources/workspace");
  return {
    workspace,
    server: path.join(workspace, "bin/studio-runtime-server"),
    renderer: path.join(
      workspace,
      platform === "linux"
        ? "workspaces/client/apps/web/dist/studio-renderer.json"
        : "web/dist/studio-renderer.json",
    ),
  };
}
const DEFAULT_URL = "http://127.0.0.1:4620";
/** Prefix of the run folder below the platform temporary directory. */
export const RUN_PREFIX = "studio-smoke-";
/** Marker file in the run folder; it holds the run id. */
export const RUN_MARKER = "studio-smoke-run.json";
/** Seconds a backend must have run before the smoke uses it. */
const BACKEND_SETTLED_SECONDS = 60;
/** Milliseconds for one HTTP request. */
const REQUEST_MS = 30_000;
/** Milliseconds for a coherent read-only usage snapshot on a busy installed backend. */
const USAGE_SNAPSHOT_MS = 120_000;
/** Milliseconds for one model turn to finish. */
const TURN_MS = 240_000;
/** Milliseconds for the monitor turn: the tool call, the command and the exit event. */
const MONITOR_MS = 300_000;
/** Milliseconds between two state polls. */
const POLL_MS = 1_000;
/**
 * Milliseconds from the sign-in card submission to its decline. The coming
 * alert feature shows a pending card only after a 10 s grace; the smoke
 * declines well before that, so the owner gets no alert.
 */
const CARD_DECLINE_MS = 5_000;
/** Milliseconds the smoke waits for the card in the client state before it declines it anyway. */
const CARD_SHOWN_MS = 3_000;
/** Milliseconds for a stopped test chat to end its turn before the smoke archives it. */
const STOP_MS = 60_000;
/** Milliseconds for an archive to reach the client state. */
const ARCHIVE_SHOWN_MS = 10_000;
/** RFC 4122 URL namespace of the sign-in card id. */
const URL_NAMESPACE = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
/** Pricing owner: the models.dev artifact that the backend reads in its state folder. */
const PRICING_ARTIFACT = "model-pricing/models-dev-v1.json";
const TERMINAL_STATUSES = new Set([
  "completed",
  "idle",
  "failed",
  "interrupted",
  "blocked",
  "paused",
]);
const FAILED_STATUSES = new Set(["failed", "interrupted", "blocked", "paused"]);
/** A device-code page: a provider link without a loopback redirect needs no listener. */
const CARD_URL = "https://device.sso.us-east-1.amazonaws.com/?user_code=";

export class CheckFailed extends Error {}

/** Results of the named checks, in order. */
export class Report {
  rows: ReportRow[];
  constructor() {
    this.rows = [];
  }

  async run(name: string, check: () => unknown | Promise<unknown>) {
    const started = Date.now();
    try {
      const detail = await check();
      this.rows.push({
        name,
        status: "pass",
        detail: String(detail ?? ""),
        ms: Date.now() - started,
      });
      return true;
    } catch (error) {
      const detail =
        error instanceof CheckFailed
          ? error.message
          : String(error instanceof Error ? error.stack : error);
      this.rows.push({
        name,
        status: "FAIL",
        detail,
        ms: Date.now() - started,
      });
      return false;
    }
  }

  skip(name: string, reason: string) {
    this.rows.push({ name, status: "skip", detail: reason, ms: 0 });
  }

  failed() {
    return this.rows.filter((row) => row.status === "FAIL");
  }

  print(stream = process.stdout) {
    const width = Math.max(...this.rows.map((row) => row.name.length), 4);
    for (const row of this.rows) {
      const seconds = (row.ms / 1000).toFixed(1).padStart(6);
      stream.write(
        `${row.status.padEnd(4)} ${row.name.padEnd(width)} ${seconds} s  ${row.detail}\n`,
      );
    }
  }
}

/** Seconds in a ps `etime` value: [[dd-]hh:]mm:ss. */
export function elapsedSeconds(value: string) {
  const match = /^\s*(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+)\s*$/.exec(value);
  if (!match) {
    throw new CheckFailed(`cannot read process age "${value}"`);
  }
  const [, days = "0", hours = "0", minutes, seconds] = match;
  return (
    ((Number(days) * 24 + Number(hours)) * 60 + Number(minutes)) * 60 +
    Number(seconds)
  );
}

/**
 * The cheapest offered model of `provider` by models.dev input plus output price.
 * `models` are `/api/models` rows; a Claude alias row prices by its `resolvedModel`.
 */
export function cheapestModel(
  models: Schema["ProviderModel"][],
  pricing: Pick<PricingArtifact, "catalog">,
  provider: Provider,
) {
  const priced = pricingModels(pricing, provider);
  let best: {
    model: string;
    price: number;
    efforts: Schema["JsonValue"][];
  } | null = null;
  for (const row of models) {
    if (row?.hidden || typeof row?.model !== "string") {
      continue;
    }
    const cost =
      priced[row.model]?.cost ??
      (typeof row.resolvedModel === "string"
        ? priced[row.resolvedModel]?.cost
        : undefined);
    if (
      !cost ||
      typeof cost.input !== "number" ||
      typeof cost.output !== "number"
    ) {
      continue;
    }
    const price = cost.input + cost.output;
    if (!best || price < best.price) {
      best = {
        model: row.model,
        price,
        efforts: Array.isArray(row.supportedReasoningEfforts)
          ? row.supportedReasoningEfforts
          : [],
      };
    }
  }
  return best;
}

/** The lowest reasoning effort that a model row lists, or undefined. */
export function lowestEffort(efforts: readonly Schema["JsonValue"][]) {
  const order = ["minimal", "low", "medium", "high"];
  const names = efforts.map((entry) =>
    typeof entry === "string"
      ? entry
      : entry && typeof entry === "object" && !Array.isArray(entry)
        ? entry.reasoningEffort
        : undefined,
  );
  return order.find((name) => names.includes(name));
}

/** Transcript items that carry the user message `id`. */
export function messageItems(items: Transcript[], id: string) {
  return items.filter(
    (item) =>
      item?.role === "user" &&
      (item.id?.endsWith(`:${id}`) ||
        item.clientMessageId === id ||
        (Array.isArray(item.inputs) &&
          item.inputs.some((input) => input?.id === id))),
  );
}

/**
 * Item ids of a pulled transcript scope, in display order: a full page lists
 * its `items`; a stream delta lists the whole `order`.
 */
export function syncTranscriptIds(
  documents: Optional<Schema["SyncDocument"]>[],
  agent: string,
) {
  const head = documents.find(
    (document) => document.id === `transcript:${agent}` && !document._deleted,
  );
  if (!head) {
    throw new CheckFailed(
      `the transcript pull has no document transcript:${agent}`,
    );
  }
  const payload: Schema["TranscriptStreamDelta"] = JSON.parse(
    head.payload ?? "null",
  );
  if (Array.isArray(payload.order)) {
    return payload.order;
  }
  return (payload.items ?? []).map((item) => item.id);
}

/**
 * The request id of the sign-in card for a browser bridge item. The owner is
 * `browser_cards::card_id` in runtime-services: a UUID v5 in the URL namespace
 * of `studio-browser-open:<bridge id>`. A tooling test checks the owner.
 */
export function browserCardId(bridgeId: string) {
  const namespace = Buffer.from(URL_NAMESPACE.replaceAll("-", ""), "hex");
  const bytes = createHash("sha1")
    .update(namespace)
    .update(`studio-browser-open:${bridgeId}`)
    .digest()
    .subarray(0, 16);
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x50;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20),
  ].join("-");
}

/**
 * Delete the run folder only when it is the exact folder that this run made:
 * a direct child of the temporary root with the run prefix and a marker that
 * holds this run id. Never by pattern.
 */
export function removeRunRoot(
  root: string,
  runId: string,
  temporaryRoot: string,
) {
  const resolved = fs.realpathSync(root);
  const parent = fs.realpathSync(temporaryRoot);
  if (
    path.dirname(resolved) !== parent ||
    !path.basename(resolved).startsWith(RUN_PREFIX)
  ) {
    throw new CheckFailed(`${root} is not a run folder below ${parent}`);
  }
  const marker: Pick<Evidence, "runId"> = JSON.parse(
    fs.readFileSync(path.join(resolved, RUN_MARKER), "utf8"),
  );
  if (marker.runId !== runId) {
    throw new CheckFailed(
      `${root} belongs to run ${marker.runId}, not ${runId}`,
    );
  }
  fs.rmSync(resolved, { recursive: true, force: false });
  return resolved;
}

function sleep(ms: number) {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}

async function until<T>(
  label: string,
  ms: number,
  probe: () => Promise<Probe<T>>,
  changed?: ((ms: number) => Promise<void>) | undefined,
): Promise<T | undefined> {
  const deadline = Date.now() + ms;
  let last;
  while (Date.now() < deadline) {
    last = await probe();
    if (last?.done) {
      return last.value;
    }
    if (changed) await changed(deadline - Date.now());
    else await sleep(POLL_MS);
  }
  throw new CheckFailed(
    `${label}: timed out after ${Math.round(ms / 1000)} s${last?.note ? ` (${last.note})` : ""}`,
  );
}

function sha256File(file: string) {
  return createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function command(file: string, args: string[]) {
  return execFileSync(file, args, {
    encoding: "utf8",
    timeout: 10_000,
    stdio: ["ignore", "pipe", "ignore"],
  });
}

/** PIDs of processes that have `file` open, by lsof; empty when none. */
function lockHolders(file: string) {
  if (!fs.existsSync(file)) {
    return [];
  }
  try {
    return command(
      process.platform === "linux" ? "/usr/bin/lsof" : "/usr/sbin/lsof",
      ["-t", file],
    )
      .split(/\s+/)
      .filter(Boolean)
      .map(Number);
  } catch (error) {
    // lsof exits 1 when no process has the file open.
    if (
      error &&
      typeof error === "object" &&
      "status" in error &&
      error.status === 1
    ) {
      return [];
    }
    throw error;
  }
}

/** The live backend over HTTP, as the renderer reaches it from the loopback origin. */
class Backend {
  url: string;
  token: string;
  signal: AbortSignal | undefined;
  analyticsTimings: Timing[];
  constructor(url: string) {
    this.url = url.replace(/\/$/, "");
    this.token = "";
    this.analyticsTimings = [];
  }

  async request<T>(
    method: string,
    route: string,
    body?: unknown,
    ms = REQUEST_MS,
  ): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    if (body !== undefined) {
      headers["content-type"] = "application/json";
    }
    if (method !== "GET" && method !== "HEAD") {
      if (!this.token) {
        throw new CheckFailed(`no session token for ${method} ${route}`);
      }
      headers["x-canvas-token"] = this.token;
    }
    const init: RequestInit = {
      method,
      headers,
      redirect: "manual",
      signal: this.signal
        ? AbortSignal.any([this.signal, AbortSignal.timeout(ms)])
        : AbortSignal.timeout(ms),
    };
    if (body !== undefined) {
      init.body = JSON.stringify(body);
    }
    let response;
    try {
      response = await fetch(this.url + route, init);
    } catch (error) {
      throw new CheckFailed(`${method} ${route}: ${error}`);
    }
    let text;
    try {
      text = await response.text();
    } catch (error) {
      throw new CheckFailed(`${method} ${route} response: ${error}`);
    }
    if (!response.ok) {
      throw new CheckFailed(
        `${method} ${route}: HTTP ${response.status} ${text.slice(0, 300)}`,
      );
    }
    return JSON.parse(text || "null");
  }

  async get<T>(route: string, ms?: number): Promise<T> {
    if (!route.startsWith("/api/analytics?"))
      return this.request<T>("GET", route, undefined, ms);
    const timing: Timing = {
      route,
      startedAt: new Date().toISOString(),
      timeoutMs: ms ?? REQUEST_MS,
    };
    const start = Date.now();
    try {
      const value = await this.request<T>("GET", route, undefined, ms);
      timing.outcome = "success";
      return value;
    } catch (error) {
      timing.outcome = /timeout|timed out/i.test(
        error instanceof Error ? error.message : String(error),
      )
        ? "timeout"
        : "error";
      throw error;
    } finally {
      timing.elapsedMs = Date.now() - start;
      this.analyticsTimings.push(timing);
    }
  }

  post<T = Schema["JsonValue"]>(
    route: string,
    body: unknown,
    ms?: number,
  ): Promise<T> {
    return this.request<T>("POST", route, body, ms);
  }
}

/** SSE invalidations drive all usage-scenario reads; no interval fetches. */
export class LiveChanges {
  controller: AbortController;
  signal: AbortSignal | undefined;
  connectMs: number;
  version: number;
  consumed: number;
  waiters: Set<() => void>;
  entities: Map<string, LiveEntity>;
  ready: Promise<void>;
  reader: Promise<void> | undefined;
  error: unknown;
  constructor(
    backend: Pick<Backend, "url">,
    agent: string,
    protocol: number,
    {
      signal,
      connectMs = REQUEST_MS,
    }: { signal?: AbortSignal; connectMs?: number } = {},
  ) {
    this.controller = new AbortController();
    this.signal = signal;
    this.connectMs = connectMs;
    this.version = 0;
    this.consumed = 0;
    this.waiters = new Set();
    this.entities = new Map();
    this.ready = this.read(backend, agent, protocol);
  }

  async read(backend: Pick<Backend, "url">, agent: string, protocol: number) {
    const resources = [
      { kind: "state" },
      { kind: "transcript", agentId: agent },
      { kind: "agentDetail", agentId: agent },
      { kind: "session-cost", agentId: agent },
      { kind: "monitors", agentId: agent },
    ];
    const timer = setTimeout(
      () => this.controller.abort(new CheckFailed("SSE connection timed out")),
      this.connectMs,
    );
    let response;
    try {
      response = await fetch(
        `${backend.url}/api/sync/stream?protocol=${protocol}&resources=${encodeURIComponent(JSON.stringify(resources))}`,
        {
          signal: this.signal
            ? AbortSignal.any([this.controller.signal, this.signal])
            : this.controller.signal,
          headers: { accept: "text/event-stream" },
        },
      );
    } finally {
      clearTimeout(timer);
    }
    if (!response.ok) throw new CheckFailed(`SSE HTTP ${response.status}`);
    this.reader = this.consume(response.body);
  }

  async consume(body: ReadableStream<Uint8Array> | null) {
    let buffer = "";
    const decoder = new TextDecoder();
    try {
      for await (const chunk of body ?? []) {
        buffer += decoder
          .decode(chunk, { stream: true })
          .replaceAll("\r\n", "\n");
        let end;
        while ((end = buffer.indexOf("\n\n")) >= 0) {
          const frame = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          const data = frame
            .split("\n")
            .filter((line) => line.startsWith("data:"))
            .map((line) => line.slice(5).trim())
            .join("\n");
          if (!data || !frame.includes("event: resources")) continue;
          const event: Schema["ResourceChangeEvent"] = JSON.parse(data);
          for (const entry of event.resourceVersions ?? []) {
            for (const doc of entry.entityChanges?.documents ?? []) {
              if (doc.op !== "delete" && "value" in doc)
                this.entities.set(`${doc.collection}:${doc.id}`, doc.value);
            }
          }
          this.version++;
          for (const wake of this.waiters) wake();
        }
      }
      if (!this.controller.signal.aborted)
        throw new Error("Usage SSE closed unexpectedly");
    } catch (error) {
      if (!this.controller.signal.aborted) this.error = error;
      for (const wake of this.waiters) wake();
    }
  }

  async changed(ms: number) {
    this.signal?.throwIfAborted();
    if (this.error) throw this.error;
    if (this.version > this.consumed) {
      this.consumed = this.version;
      return;
    }
    await new Promise<void>((resolve) => {
      const wake = () => {
        clearTimeout(timer);
        this.waiters.delete(wake);
        this.signal?.removeEventListener("abort", wake);
        resolve();
      };
      const timer = setTimeout(wake, Math.max(0, ms));
      this.waiters.add(wake);
      this.signal?.addEventListener("abort", wake, { once: true });
    });
    this.signal?.throwIfAborted();
    this.consumed = this.version;
    if (this.error) throw this.error;
  }

  async close() {
    this.controller.abort();
    await this.reader;
  }
}

export function analyticsRoute(id: string) {
  return `/api/analytics?agent=${encodeURIComponent(id)}&scope=agent&limit=500`;
}

export class Smoke {
  cancel: AbortController;
  app: string;
  usage: boolean | undefined;
  sqliteRoAgent: string | undefined;
  usageEvidence: Evidence;
  backend: Backend;
  model: Record<string, string>;
  providers: string[] | undefined;
  report: Report;
  runId: string;
  temporaryRoot: string;
  root: string | null;
  rootIdentity: DirectoryIdentity | undefined;
  projectRegistered: boolean;
  chats: string[];
  openCards: Map<string, string>;
  ownerChats: Map<
    string,
    Optional<Pick<Schema["AgentSummaryEntityDto"], "name" | "archived">>
  >;
  desktop: Schema["DesktopResponse"] | null;
  state: string | null;
  live: Pick<LiveChanges, "entities" | "changed" | "close"> | null = null;
  accountRecords = new Map<string, Schema["Account"]>();
  accountsByProvider = new Map<Provider, string>();
  evidencePath: string | undefined;
  interrupted = false;
  constructor(options: Options) {
    this.cancel = new AbortController();
    this.app = options.app;
    this.usage = options.usage;
    this.sqliteRoAgent = options["sqlite-ro-agent"];
    this.usageEvidence = {
      format: "studio-live-usage-v1",
      runId: null,
      startedAt: new Date().toISOString(),
      providers: [],
    };
    this.backend = new Backend(options.url);
    this.backend.signal = this.cancel.signal;
    this.model = options.model;
    this.providers = options.providers;
    this.report = new Report();
    this.runId = randomBytes(4).toString("hex");
    this.usageEvidence.runId = this.runId;
    this.temporaryRoot = fs.realpathSync(os.tmpdir());
    this.root = null;
    this.projectRegistered = false;
    this.chats = [];
    /** Sign-in cards that this run submitted and did not decline yet: request id to chat. */
    this.openCards = new Map();
    this.ownerChats = new Map();
    this.desktop = null;
    this.state = null;
  }

  checkActive() {
    this.cancel.signal.throwIfAborted();
  }

  async submitModel(
    route: string,
    body: { id: string; room?: string; [key: string]: unknown },
  ) {
    this.checkActive();
    if (this.usage && this.root) {
      if (
        this.rootIdentity &&
        JSON.stringify(directoryIdentity(this.root)) !==
          JSON.stringify(this.rootIdentity)
      )
        throw new CheckFailed(
          "Run folder identity changed before model submission",
        );
      this.usageEvidence.submissions ??= [];
      this.usageEvidence.submissions.push({
        route,
        id: body.id,
        room: body.room,
        at: new Date().toISOString(),
      });
      this.saveUsage();
    }
    return this.backend.post(route, body);
  }

  async bridgeObservation(accountKey: string) {
    assert.ok(this.state);
    assert.ok(this.desktop);
    const desktop =
      await this.backend.get<Schema["DesktopResponse"]>("/api/desktop");
    if (desktop.stateDir !== this.state)
      throw new CheckFailed("Bridge observation changed state directory");
    const release = fs.realpathSync(this.workspace());
    const releases = [this.workspace(), release];
    if (process.platform === "linux") {
      const parent = path.dirname(release);
      for (const entry of fs.readdirSync(parent, { withFileTypes: true }))
        if (entry.isDirectory()) releases.push(path.join(parent, entry.name));
    }
    const bridges = [
      ...new Set(
        releases.flatMap((root) => [
          path.join(root, "workspaces/providers/apps/claude-bridge/bridge.mjs"),
          path.join(root, "scripts/claude_bridge/bridge.mjs"),
        ]),
      ),
    ];
    const accountRoot =
      accountKey === "default"
        ? this.state
        : path.join(this.state, "account-servers", accountKey);
    const processes: ProcessRow[] = command("/bin/ps", [
      "-axww",
      "-o",
      "pid=,ppid=,command=",
    ])
      .split("\n")
      .flatMap((line) => {
        const match = /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line);
        return match
          ? [
              {
                pid: Number(match[1]),
                ppid: Number(match[2]),
                command: match[3] ?? "",
              },
            ]
          : [];
      });
    if (desktop.supervisorMode) {
      for (const row of processes)
        if (
          row.command.includes("bridge.mjs") &&
          row.command.endsWith(` ${accountRoot}`)
        ) {
          try {
            row.startTime = processStartIdentity(row.pid);
          } catch {
            row.startTime = null;
          }
        }
    }
    const matches = bridgeProcessMatches(
      processes,
      bridges,
      accountRoot,
      desktop.pid,
      {
        enabled: desktop.supervisorMode,
        health:
          desktop.supervisor && "stateDir" in desktop.supervisor
            ? desktop.supervisor
            : null,
        accountKey,
        stateDir: this.state,
      },
    );
    if (!matches.length)
      throw new CheckFailed(
        `No Claude bridge owned by server ${this.desktop.pid} for exact root ${accountRoot}`,
      );
    return matches.map((row) => ({
      ...row,
      observedAt: new Date().toISOString(),
      release: row.bridge.endsWith("/scripts/claude_bridge/bridge.mjs")
        ? row.bridge.slice(0, -"/scripts/claude_bridge/bridge.mjs".length)
        : row.bridge.slice(
            0,
            -"/workspaces/providers/apps/claude-bridge/bridge.mjs".length,
          ),
      sha256: sha256File(row.bridge),
    }));
  }

  workspace() {
    return installationPaths(this.app).workspace;
  }

  async preflight() {
    const desktop =
      await this.backend.get<Schema["DesktopResponse"]>("/api/desktop");
    if (
      desktop?.application !== "codex-agents" ||
      desktop.protocol !== 1 ||
      desktop.backendKind !== "rust"
    ) {
      throw new CheckFailed(
        "the server at this address is not a Rust Codex Studio backend",
      );
    }
    if (!Number.isInteger(desktop.pid) || desktop.pid < 1) {
      throw new CheckFailed("the backend reports no process ID");
    }
    process.kill(desktop.pid, 0);
    const age = elapsedSeconds(
      command("/bin/ps", ["-p", String(desktop.pid), "-o", "etime="]),
    );
    if (age < BACKEND_SETTLED_SECONDS) {
      throw new CheckFailed(
        `the backend started ${age} s ago; it may still be starting`,
      );
    }
    if (process.platform === "linux") {
      const servicePid = Number(
        command("systemctl", [
          "--user",
          "show",
          "codex-studio",
          "--property=MainPID",
          "--value",
        ]).trim(),
      );
      if (servicePid !== desktop.pid)
        throw new CheckFailed(
          `systemd codex-studio pid ${servicePid} differs from backend ${desktop.pid}`,
        );
    }
    const state = desktop.stateDir;
    const holders = lockHolders(path.join(state, "runtime.lock"));
    if (holders.some((pid) => pid !== desktop.pid)) {
      throw new CheckFailed(
        `another process holds ${state}/runtime.lock: ${holders.join(", ")}`,
      );
    }
    const processes = command("/bin/ps", ["-axww", "-o", "pid=,command="])
      .split("\n")
      .map((line) => /^\s*(\d+)\s+(.*)$/.exec(line))
      .filter((match) => match !== null)
      .map(([, pid, line]) => ({ pid: Number(pid), line: line ?? "" }));
    // A second backend of this bundle is starting, or waits for the runtime lock.
    const server = installationPaths(this.app).server;
    const others = processes.filter(
      (row) =>
        (row.line.startsWith(server) ||
          row.line.startsWith(fs.realpathSync(server))) &&
        row.pid !== desktop.pid,
    );
    if (others.length > 0) {
      throw new CheckFailed(
        `another backend of this app runs: pid ${others.map((row) => row.pid).join(", ")}`,
      );
    }
    const client = path.join(this.app, "Contents", "MacOS") + path.sep;
    const instances = processes
      .filter((row) => row.line.startsWith(client))
      .map((row) => String(row.pid));
    if (instances.length > 1) {
      throw new CheckFailed(
        `${instances.length} app instances run: pid ${instances.join(", ")}`,
      );
    }
    const summary =
      await this.backend.get<Schema["UiSummaryResponse"]>("/api/ui-summary");
    if (summary?.ready !== true) {
      throw new CheckFailed("the backend summary is not ready");
    }
    for (const chat of summary.chats ?? []) {
      this.ownerChats.set(chat.id, {
        name: chat.name,
        archived: chat.archived,
      });
    }
    this.desktop = desktop;
    this.state = state;
    return `backend pid ${desktop.pid} up ${age} s, state ${state}, ${this.ownerChats.size} owner chats`;
  }

  async build() {
    assert.ok(this.desktop);
    const server = installationPaths(this.app).server;
    const running =
      process.platform === "linux"
        ? fs.readlinkSync(`/proc/${this.desktop.pid}/exe`)
        : command("/bin/ps", [
            "-p",
            String(this.desktop.pid),
            "-o",
            "comm=",
          ]).trim();
    if (fs.realpathSync(running) !== fs.realpathSync(server)) {
      throw new CheckFailed(`the backend runs ${running}, not ${server}`);
    }
    const digest = sha256File(server);
    if (digest !== this.desktop.backendBuild) {
      throw new CheckFailed(
        `the backend reports build ${this.desktop.backendBuild}, the installed server is ${digest}`,
      );
    }
    return `installed server ${digest.slice(0, 12)} runs`;
  }

  async renderer() {
    const served = await this.backend.get<{ build: string; apiSchema: string }>(
      "/studio-renderer.json",
    );
    const installed: RendererManifest = JSON.parse(
      fs.readFileSync(installationPaths(this.app).renderer, "utf8"),
    );
    if (
      served?.build !== installed.build ||
      served?.apiSchema !== installed.apiSchema
    ) {
      throw new CheckFailed(
        `the served renderer ${served?.build} is not the installed ${installed.build}`,
      );
    }
    return `renderer ${served.build} served`;
  }

  /** One ready account per provider, preferring the default account. */
  async accounts() {
    const listing =
      await this.backend.get<Schema["AccountsResponse"]>("/api/accounts");
    const ready = (listing.accounts ?? []).filter(
      (account) =>
        account.status === "ready" && !account.disconnected && !account.deleted,
    );
    const chosen = new Map<Provider, string>();
    for (const account of ready) {
      const provider = account.provider ?? "codex";
      if (this.providers && !this.providers.includes(provider)) {
        continue;
      }
      if (!chosen.has(provider) || account.id === listing.defaultAccountKey) {
        chosen.set(provider, account.id);
      }
    }
    if (chosen.size === 0) {
      throw new CheckFailed("no provider has a ready login");
    }
    if (this.usage && (!chosen.has("codex") || !chosen.has("claude")))
      throw new CheckFailed("--usage requires ready Claude and Codex accounts");
    this.accountRecords = new Map(
      ready.map((account) => [account.id, account]),
    );
    this.accountsByProvider = chosen;
    return [...chosen]
      .map(([provider, account]) => `${provider}: ${account}`)
      .join(", ");
  }

  async session() {
    const session =
      await this.backend.get<Schema["SessionResponse"]>("/api/session");
    if (typeof session?.token !== "string" || !session.token) {
      throw new CheckFailed("the backend gave no session token");
    }
    this.backend.token = session.token;
    return "session token read";
  }

  makeRoot() {
    this.root = fs.mkdtempSync(path.join(this.temporaryRoot, RUN_PREFIX));
    this.rootIdentity = directoryIdentity(this.root);
    fs.writeFileSync(
      path.join(this.root, RUN_MARKER),
      JSON.stringify({
        runId: this.runId,
        pid: process.pid,
        created: new Date().toISOString(),
      }),
    );
    fs.writeFileSync(path.join(this.root, NOTES_FILE), NOTES_MARKER + "\n", {
      mode: 0o600,
    });
    return this.root;
  }

  async chooseModel(provider: Provider, account: string) {
    assert.ok(this.state);
    if (this.model?.[provider]) {
      return { model: this.model[provider], efforts: [] };
    }
    const pricing: PricingArtifact = JSON.parse(
      fs.readFileSync(path.join(this.state, PRICING_ARTIFACT), "utf8"),
    );
    const listing = await this.backend.get<Schema["ModelCatalogResponse"]>(
      `/api/models?account_key=${encodeURIComponent(account)}`,
      60_000,
    );
    const best = cheapestModel(listing?.data ?? [], pricing, provider);
    if (!best) {
      throw new CheckFailed(
        `no priced ${provider} model is offered; pass --model ${provider}=<id>`,
      );
    }
    return best;
  }

  async transcript(agent: string) {
    const page = await this.backend.get<Schema["TranscriptPageResponse"]>(
      `/api/transcript/page?id=${encodeURIComponent(agent)}&limit=200`,
    );
    return page?.items ?? [];
  }

  async chatStatus(agent: string) {
    const summary =
      await this.backend.get<Schema["UiSummaryResponse"]>("/api/ui-summary");
    return (summary.chats ?? []).find((chat) => chat.id === agent);
  }

  /** Wait until the chat's turn ends and an assistant item contains `token`. */
  async answered(agent: string, token: string, ms: number) {
    return until(
      `answer ${token}`,
      ms,
      async () => {
        this.checkActive();
        const chat = await this.chatStatus(agent);
        const items = await this.transcript(agent);
        const answer = items.find(
          (item) => item.role === "assistant" && item.text?.includes(token),
        );
        if (chat && !chat.inFlight && FAILED_STATUSES.has(chat.status)) {
          throw new CheckFailed(`the chat is ${chat.status}`);
        }
        const settled =
          chat && !chat.inFlight && TERMINAL_STATUSES.has(chat.status);
        return {
          done: Boolean(answer && settled),
          value: items,
          note: `status ${chat?.status}`,
        };
      },
      this.live
        ? (ms) => this.live?.changed(ms) ?? Promise.resolve()
        : undefined,
    );
  }

  permissionRecord(id: string) {
    assert.ok(this.state);
    return sqliteEvidence(this.state, id, this.runId).permissions;
  }

  async analyticsSnapshot(id: string, ms?: number) {
    assert.ok(this.state || this.sqliteRoAgent !== id);
    if (this.sqliteRoAgent === id) {
      assert.ok(this.state);
      return sqliteEvidence(this.state, id, this.runId, true).analytics;
    }
    const before = this.backend.analyticsTimings.length;
    try {
      return await this.backend.get<Schema["AnalyticsResponse"]>(
        analyticsRoute(id),
        ms,
      );
    } finally {
      const timings = this.backend.analyticsTimings.slice(before);
      if (timings.length) {
        this.usageEvidence.analyticsLatency ??= [];
        this.usageEvidence.analyticsLatency.push(
          ...timings.map((timing) => ({ ...timing, agentId: id })),
        );
        this.saveUsage();
      }
    }
  }

  async turn(provider: Provider) {
    assert.ok(this.root);
    assert.ok(this.rootIdentity);
    const root = this.root;
    const account = this.accountsByProvider.get(provider);
    assert.ok(account);
    const { model, efforts } = await this.chooseModel(provider, account);
    const id = randomUUID();
    const token = `SMOKE_${this.runId}_${provider.toUpperCase()}`;
    // Archive this chat even when the create response is lost.
    this.chats.push(id);
    if (this.usage) this.saveUsage();
    const effort = lowestEffort(efforts);
    this.checkActive();
    if (this.usage) {
      if (provider === "claude") {
        this.usageEvidence.servingBridges ??= {};
        this.usageEvidence.servingBridges[account] ??=
          await this.bridgeObservation(account);
        this.saveUsage();
      }
      const protocol =
        await this.backend.get<Schema["SyncProtocolResponse"]>(
          "/api/sync/protocol",
        );
      const live = new LiveChanges(this.backend, id, protocol.protocolVersion, {
        signal: this.cancel.signal,
      });
      this.live = live;
      await live.ready;
    }
    if (this.usage) {
      this.checkActive();
      const created = await this.backend.post<Schema["AgentRecordResponse"]>(
        "/api/leads",
        {
          id,
          cwd: this.root,
          account_key: account,
          model,
          ...(effort ? { effort } : {}),
          workspaceMode: "worktree",
          yolo_mode: false,
          reuse_empty: false,
        },
      );
      this.projectRegistered = true;
      await this.backend.post("/api/rename", {
        id,
        name: `Studio smoke ${this.runId} ${provider}`,
        request_id: `smoke-name-${this.runId}-${provider}`,
      });
      const permission = await until(
        "idle chat permission record",
        REQUEST_MS,
        async () => {
          this.checkActive();
          const detail = this.live?.entities.get(`agentDetail:${id}`);
          return {
            done: Boolean(detail),
            value: detail
              ? verifiedPermissions(
                  { ...created, ...detail, ...this.permissionRecord(id) },
                  provider,
                  root,
                  this.rootIdentity,
                )
              : null,
          };
        },
        (ms) => this.live?.changed(ms) ?? Promise.resolve(),
      );
      this.usageEvidence.permissions ??= {};
      this.usageEvidence.permissions[id] = permission;
      this.saveUsage();
      await this.submitModel("/api/messages", {
        id: randomUUID(),
        room: id,
        delivery: "next_turn",
        text: `This is an automated Studio smoke check. Do not use tools. Reply with exactly ${token}.`,
      });
    } else {
      await this.submitModel("/api/agents", {
        id,
        name: `Studio smoke ${this.runId} ${provider}`,
        prompt: `This is an automated Studio smoke check. Do not use tools. Reply with exactly ${token}.`,
        cwd: this.root,
        account_key: account,
        model,
        ...(effort ? { effort } : {}),
        role: "orchestrator",
        workspaceMode: "worktree",
      });
    }
    this.projectRegistered = true;
    await this.answered(id, token, TURN_MS);
    return {
      id,
      model,
      account,
      detail: `${model}${effort ? ` ${effort}` : ""} answered on ${account}`,
    };
  }

  async usageScenario(chat: Chat, provider: Provider) {
    assert.ok(this.root);
    this.checkActive();
    const { id } = chat;
    const token = `SMOKE_${this.runId}_${provider}_LONG`;
    const inputId = randomUUID();
    const initialTurnId =
      provider === "claude"
        ? (await this.transcript(id))
            .filter((item) => item.role === "assistant")
            .at(-1)?.turnId
        : null;
    await this.submitModel("/api/messages", {
      id: inputId,
      room: id,
      delivery: "next_turn",
      text: `This is turn 2 of a usage check. Write about 150 words explaining why small deterministic tests are useful. Then use one read-only file-read tool to read ${path.join(this.root, NOTES_FILE)} (if no file-read tool exists, run cat on that exact file). After that tool result write another 150 words about test cleanup. Do not read any other file, delegate, or create monitors. End with ${token}.`,
    });
    let steer: Steer | null = null;
    if (provider === "claude") {
      const activeTurnId = await until(
        "Claude turn 2 starts",
        TURN_MS,
        async () => {
          this.checkActive();
          const status = await this.chatStatus(id);
          const active = this.live?.entities.get(`agentSummary:${id}`);
          return {
            done:
              status?.inFlight === true &&
              active?.turnId &&
              active.turnId !== initialTurnId,
            value: active?.turnId,
            note: status?.status,
          };
        },
        (ms) => this.live?.changed(ms) ?? Promise.resolve(),
      );
      steer = {
        id: randomUUID(),
        turnId: activeTurnId,
        sentAt: new Date().toISOString(),
      };
      this.usageEvidence.scenarios ??= {};
      this.usageEvidence.scenarios[id] = { steer, inputId };
      this.saveUsage();
      steer.receipt = await this.submitModel("/api/messages", {
        id: steer.id ?? "",
        room: id,
        delivery: "current_turn",
        text: "Keep going with the same requested 300-word answer and the single read-only note read; mention reproducibility. This is the usage-check steer, not a new task.",
      });
    }
    const longItems = await this.answered(id, token, TURN_MS);
    const finalToken = `SMOKE_${this.runId}_${provider}_FINAL`;
    await this.submitModel("/api/messages", {
      id: randomUUID(),
      room: id,
      delivery: "next_turn",
      text: `Turn 3: reply with exactly ${finalToken}. Do not use tools.`,
    });
    await this.answered(id, finalToken, TURN_MS);
    return this.collectUsage(chat, provider, {
      steer,
      longTurn: { inputId, items: longItems },
    });
  }

  async collectUsage(
    chat: Chat,
    provider: Provider,
    scenario: {
      entities?: OwnedEntities;
      savedBridge?: BridgeObservation[] | undefined;
      steer?: Steer | null;
      longTurn?: { inputId: string; items: Transcript[] | undefined };
    } = {},
  ) {
    assert.ok(this.state);
    assert.ok(this.root);
    assert.ok(chat.account);
    const account = this.accountRecords.get(chat.account);
    assert.ok(account);
    const { id } = chat;
    console.log(`Collecting ${provider} usage for ${id}`);
    const transcript = await this.transcript(id);
    const finalTurnId = transcript
      .filter((item) => item.role === "assistant")
      .at(-1)?.turnId;
    const entities =
      scenario.entities ?? (await this.ownEntities(id, finalTurnId));
    assert.ok(entities);
    const analytics = entities.analytics;
    const stored = { agent: entities.agent, history: analytics.history ?? [] };
    const truth = nativeEvidence(
      this.state,
      stored.agent,
      account,
      stored.history,
    );
    Object.assign(
      truth,
      priceResponses(
        truth.responses,
        JSON.parse(
          fs.readFileSync(path.join(this.state, PRICING_ARTIFACT), "utf8"),
        ),
        provider,
      ),
    );
    const recorded = recordedUsage(analytics);
    let reconciliation = null;
    try {
      reconciliation = await this.backend.get<
        Schema["UsageReconciliationResponse"]
      >(`/api/diagnostics/usage-reconciliation?agent=${id}`);
    } catch (error) {
      if (
        !(error instanceof Error ? error.message : String(error)).includes(
          "HTTP 404",
        )
      )
        throw error;
    }
    const displayedCost = entities.sessionCost;
    const previous = this.usageEvidence.providers.find(
      (row) => row.agentId === id,
    );
    const savedBridge =
      this.usageEvidence.servingBridges?.[chat.account] ??
      scenario.savedBridge ??
      previous?.bridgeProcesses;
    const provenance =
      provider === "claude"
        ? await this.currentBridgeProvenance(chat.account, savedBridge)
        : {
            serving: [],
            current: { available: false, reason: "not a Claude run" },
          };
    const bridgeProcesses = provenance.serving;
    const proof = scenarioEvidence(transcript, provider, this.root);
    if (provider === "claude") {
      const item = transcript.find((item) =>
        item.inputs?.some((input) => input.text?.includes("usage-check steer")),
      );
      const input = item?.inputs?.find((input) =>
        input.text?.includes("usage-check steer"),
      );
      const submitted = scenario.steer ??
        this.usageEvidence.scenarios?.[id]?.steer ?? {
          id: input?.id ?? undefined,
          turnId: proof.longTurnId,
          association:
            "recovered from materialized current-turn input within turn-2 transcript window",
        };
      const receipts = await this.backend.get<Schema["MessageReceipts"]>(
        `/api/messages/receipts?agent=${encodeURIComponent(id)}&ids=${encodeURIComponent(JSON.stringify([submitted.id]))}`,
      );
      const receipt = receipts.items?.find(
        (receipt) => receipt.id === submitted.id,
      );
      const startedAt = transcript.find(
        (item) =>
          item.role === "user" &&
          item.text?.startsWith("This is turn 2 of a usage check."),
      )?.at;
      const endedAt = Math.max(
        ...transcript
          .filter((item) => item.turnId === proof.longTurnId)
          .map((item) => Number(item.at)),
      );
      proof.steer = validateSteer(
        item,
        receipt,
        submitted,
        proof.longTurnId,
        startedAt,
        endedAt,
      );
    }
    const row = {
      publication: entities.publication,
      analyticsSource: analytics.source ?? "api",
      analyticsLatency: this.backend.analyticsTimings
        .filter(
          (timing) =>
            new URLSearchParams(timing.route.split("?")[1]).get("agent") === id,
        )
        .concat(previous?.analyticsLatency ?? []),
      scenario: proof,
      provider,
      agentId: id,
      chatId: id,
      threadId: stored.agent.threadId,
      account: chat.account,
      model: chat.model,
      steer: scenario.steer ?? null,
      longTurn: scenario.longTurn ?? null,
      transcript,
      truth,
      recorded: { ...recorded, tokens: sumTokens(recorded.responses) },
      analytics,
      budget: {
        tokensUsed: stored.agent.tokensUsed,
        tokenUsageAccounting:
          stored.agent.tokenUsageAccounting ??
          accountingEvidence(this.state, id),
      },
      displayedCost: displayedCost ?? null,
      reconciliation,
      bridgeProcesses,
      bridgeObservationAtCollection: provenance.current,
      permissions: this.usageEvidence.permissions?.[id] ??
        previous?.permissions ?? {
          verified: false,
          reason: "OLD run preceded permission verification",
        },
    };
    this.usageEvidence.providers = this.usageEvidence.providers.filter(
      (previous) => previous.agentId !== id,
    );
    this.usageEvidence.providers.push(row);
    this.saveUsage();
    if (!displayedCost)
      throw new CheckFailed("No sessionCost sync entity was observed");
    if (provider === "claude" && !bridgeProcesses.length)
      throw new CheckFailed("No Claude bridge process identity found");
    const longResponseIds = truth.responses.filter(
      (response) =>
        response.turnId && response.turnId === truth.responses.at(-2)?.turnId,
    );
    if (provider === "codex" && longResponseIds.length < 2)
      throw new CheckFailed("Turn 2 has fewer than two native model responses");
    return `${id}: ${truth.responses.length} native responses, ${recorded.responses.length} recorded responses`;
  }

  /** Match the UI publication to a fresh cost generation containing final-turn usage. */
  async ownEntities(
    id: string,
    finalTurnId?: string | null,
  ): Promise<OwnedEntities> {
    const protocol =
      await this.backend.get<Schema["SyncProtocolResponse"]>(
        "/api/sync/protocol",
      );
    const stream = new LiveChanges(this.backend, id, protocol.protocolVersion, {
      signal: this.cancel.signal,
    });
    try {
      await stream.ready;
      const deadline = Date.now() + USAGE_SNAPSHOT_MS;
      const read = <T>(route: string) =>
        this.backend.get<T>(route, Math.max(1, deadline - Date.now()));
      const analyticsSnapshot = () =>
        this.analyticsSnapshot(id, Math.max(1, deadline - Date.now()));
      const result = await until<OwnedEntities>(
        "final-turn usage publication",
        USAGE_SNAPSHOT_MS,
        async () => {
          this.checkActive();
          const analytics = await analyticsSnapshot();
          const summary = (analytics.agents ?? []).find((row) => row.id === id);
          const detail = stream.entities.get(`agentDetail:${id}`);
          const sessionCost = stream.entities.get(`sessionCost:${id}`);
          const currentCost = await read<Schema["SessionCostResponse"]>(
            `/api/session-cost?agent=${id}`,
          );
          const final =
            finalTurnId ??
            (await this.transcript(id))
              .filter((item) => item.role === "assistant")
              .at(-1)?.turnId;
          if (
            !summary ||
            !detail ||
            !sessionCost ||
            !publicationReady(sessionCost, currentCost, analytics, final)
          )
            return {
              done: false,
              note: `cost generation ${sessionCost?.generation}/${currentCost.generation}, state ${sessionCost?.pricingState}`,
            };
          const after = await analyticsSnapshot();
          return {
            done:
              JSON.stringify(after.timeline) ===
              JSON.stringify(analytics.timeline),
            value: {
              analytics: after,
              agent: { ...summary, ...detail },
              sessionCost,
              publication: {
                finalTurnId: final,
                currentCost,
                capturedAt: new Date().toISOString(),
              },
            },
          };
        },
        (ms) => stream.changed(ms),
      );
      assert.ok(result);
      return result;
    } finally {
      await stream.close();
    }
  }

  /** Recover evidence from an interrupted collector without submitting another model turn. */
  async recover(file: string) {
    const evidence = readEvidence(file);
    if (
      evidence.format !== "studio-live-usage-v1" ||
      !/^[a-f0-9]{8}$/.test(evidence.runId ?? "") ||
      !Array.isArray(evidence.chatIds)
    )
      throw new CheckFailed("Invalid usage evidence file");
    this.usageEvidence = evidence;
    assert.ok(evidence.runId);
    this.runId = evidence.runId;
    this.chats = evidence.chatIds;
    if (this.sqliteRoAgent && !this.chats.includes(this.sqliteRoAgent))
      throw new CheckFailed(
        "SQLite analytics recovery must name a chat of this exact run",
      );
    assert.ok(evidence.runFolder);
    assert.ok(evidence.server);
    this.root = evidence.runFolder;
    this.desktop =
      await this.backend.get<Schema["DesktopResponse"]>("/api/desktop");
    this.state = this.desktop.stateDir;
    if (
      this.desktop.backendBuild !== evidence.server.sha256 ||
      this.state !== evidence.server.stateDir
    )
      throw new CheckFailed(
        "Recovery requires the same backend build and state directory",
      );
    preserveProvenance(this.usageEvidence, this.serverObservation(), true);
    const accounts =
      await this.backend.get<Schema["AccountsResponse"]>("/api/accounts");
    this.accountRecords = new Map(
      accounts.accounts.map((account) => [account.id, account]),
    );
    for (const id of this.chats) {
      const completed = this.usageEvidence.providers.find(
        (row) => row.agentId === id,
      );
      if (
        completed?.publication &&
        publicationReady(
          completed.displayedCost,
          completed.publication.currentCost,
          completed.analytics ?? {},
          completed.publication.finalTurnId,
        ) &&
        (completed.provider !== "claude" ||
          (completed.scenario?.steer &&
            "receipt" in completed.scenario.steer &&
            isRecord(completed.scenario.steer.receipt) &&
            completed.scenario.steer.receipt.materialized))
      ) {
        console.log(`Keeping completed usage snapshot for ${id}`);
        continue;
      }
      console.log(`Recovering usage snapshot for ${id}`);
      const entities = await this.ownEntities(id);
      const { agent } = entities;
      assert.ok(agent.accountKey);
      assert.ok(agent.provider === "codex" || agent.provider === "claude");
      if (!agent.name?.startsWith(`Studio smoke ${this.runId} `))
        throw new CheckFailed(`Chat ${id} is not owned by run ${this.runId}`);
      this.checkActive();
      await this.collectUsage(
        { id, account: agent.accountKey, model: agent.model ?? undefined },
        agent.provider,
        {
          entities,
          savedBridge: this.usageEvidence.providers.find(
            (row) => row.agentId === id,
          )?.bridgeProcesses,
        },
      );
    }
    this.usageEvidence.recoveredAt = new Date().toISOString();
    this.usageEvidence.checks = evidence.checks;
    this.saveUsage();
    this.printUsage();
  }

  serverObservation() {
    assert.ok(this.desktop);
    return {
      pid: this.desktop.pid,
      stateDir: this.state,
      binary: fs.realpathSync(installationPaths(this.app).server),
      sha256: this.desktop.backendBuild,
      release: fs.realpathSync(this.workspace()),
    };
  }

  async currentBridgeProvenance(account: string, saved?: BridgeObservation[]) {
    try {
      const observed = await this.bridgeObservation(account);
      return {
        serving: saved ?? observed,
        current: { available: true, bridges: observed },
      };
    } catch (error) {
      if (!saved?.length) throw error;
      return {
        serving: saved,
        current: {
          available: false,
          reason: error instanceof Error ? error.message : String(error),
          at: new Date().toISOString(),
        },
      };
    }
  }

  printUsage(label = "OLD") {
    console.table(usageTable(this.usageEvidence, label));
    console.log(`Usage evidence: ${this.evidencePath}`);
  }

  saveUsage() {
    const directory = path.join(os.homedir(), ".cache/codex-studio/live-usage");
    fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
    this.evidencePath = path.join(directory, `${this.runId}.json`);
    preserveProvenance(this.usageEvidence, this.serverObservation());
    if (this.report.rows.length) this.usageEvidence.checks = this.report.rows;
    this.usageEvidence.chatIds = this.chats;
    this.usageEvidence.runFolder = this.root;
    this.usageEvidence.rootIdentity ??= this.rootIdentity;
    this.usageEvidence.updatedAt = new Date().toISOString();
    atomicWriteJson(this.evidencePath, this.usageEvidence);
  }

  async messageOnce(agent: string) {
    const id = randomUUID();
    const token = `SMOKE_${this.runId}_MESSAGE`;
    const message = {
      id,
      room: agent,
      text: `Reply with exactly ${token}. Do not use tools.`,
      delivery: "next_turn",
    };
    await this.submitModel("/api/messages", message);
    // An exact retry of the same id must not enqueue the message again.
    await this.submitModel("/api/messages", message);
    const items = await this.answered(agent, token, TURN_MS);
    const copies = messageItems(items ?? [], id);
    if (copies.length !== 1) {
      throw new CheckFailed(
        `the transcript has ${copies.length} copies of message ${id}`,
      );
    }
    const receipts = await this.backend.get<Schema["MessageReceipts"]>(
      `/api/messages/receipts?agent=${encodeURIComponent(agent)}&ids=${encodeURIComponent(JSON.stringify([id]))}`,
    );
    const receipt = (receipts?.items ?? []).find((item) => item.id === id);
    if (!receipt?.materialized) {
      throw new CheckFailed(
        `message ${id} has receipt ${JSON.stringify(receipt)}`,
      );
    }
    return `one copy, receipt ${receipt.status}`;
  }

  async entitySeq() {
    const page = await this.backend.get<Schema["SyncPullResponse"]>(
      "/api/sync/pull?scope=state:entities:v1&after=0&limit=1",
    );
    return page?.maxSeq ?? 0;
  }

  /** Entity documents of `collection` changed after `after` whose value matches `accept`. */
  async changedEntities(
    after: number,
    collection: string,
    accept: (value: LiveEntity | undefined) => boolean,
  ) {
    if (collection === "monitor" && this.live)
      return [...this.live.entities]
        .filter(([key, value]) => key.startsWith("monitor:") && accept(value))
        .map(([, value]) => value);
    const found: LiveEntity[] = [];
    let cursor = after;
    for (;;) {
      const page = await this.backend.get<Schema["SyncPullResponse"]>(
        `/api/sync/pull?scope=state:entities:v1&after=${cursor}`,
      );
      for (const document of page?.documents ?? []) {
        if (
          !(
            ("collection" in document && document.collection === collection) ||
            document.id?.startsWith(`entity:${collection}:`)
          ) ||
          ("_deleted" in document && document._deleted) ||
          ("op" in document && document.op === "delete")
        ) {
          continue;
        }
        const payload: { value: LiveEntity } =
          "value" in document
            ? { value: document.value }
            : JSON.parse("payload" in document ? document.payload : "null");
        if (accept(payload.value)) {
          found.push(payload.value);
        }
      }
      const next = page?.checkpoint?.seq ?? cursor;
      if (
        !page?.documents?.length ||
        next <= cursor ||
        next >= (page.maxSeq ?? next)
      ) {
        return found;
      }
      cursor = next;
    }
  }

  async monitor(agent: string) {
    const after = await this.entitySeq();
    const token = `SMOKE_${this.runId}_MONITOR`;
    await this.submitModel("/api/messages", {
      id: randomUUID(),
      room: agent,
      text:
        "Use the orchestration_monitor tool once to run the shell command `true`. " +
        `After Studio reports that the command exited, reply with exactly ${token}.`,
      delivery: "next_turn",
    });
    await this.answered(agent, token, MONITOR_MS);
    const monitors = await this.changedEntities(
      after,
      "monitor",
      (value) => value?.agent === agent,
    );
    const finished = monitors.filter(
      (value) => value.status === "completed" && value.exitCode === 0,
    );
    if (finished.length === 0) {
      throw new CheckFailed(
        `no completed monitor of the chat: ${JSON.stringify(monitors.map((value) => [value.status, value.exitCode]))}`,
      );
    }
    return `monitor ${finished[0]?.id ?? "?"} completed with exit 0`;
  }

  /** Submit a sign-in card the way a command's $BROWSER does, then decline it at once. */
  async signInCard(agent: string) {
    assert.ok(this.state);
    const socket = path.join(this.state, "browser-host", "open.sock");
    const after = await this.entitySeq();
    const submitted = Date.now();
    const reply = await new Promise<{
      ok?: boolean;
      id?: string;
      message?: string;
    }>((resolve, reject) => {
      const connection = net.createConnection(socket);
      let data = "";
      connection.setTimeout(REQUEST_MS, () =>
        connection.destroy(new Error("the sign-in socket timed out")),
      );
      connection.on("connect", () =>
        connection.write(
          `${JSON.stringify({ v: 1, url: `${CARD_URL}SMOKE-${this.runId}`, session: agent })}\n`,
        ),
      );
      connection.on("data", (chunk) => {
        data += chunk;
        if (data.includes("\n")) {
          connection.end();
          resolve(JSON.parse(data));
        }
      });
      connection.on("error", reject);
    });
    if (reply?.ok !== true || typeof reply.id !== "string") {
      throw new CheckFailed(
        `the sign-in socket refused the card: ${reply?.message}`,
      );
    }
    const id = browserCardId(reply.id);
    this.openCards.set(id, agent);
    // The client shows a card through its pending request entity.
    let shown = false;
    try {
      shown = Boolean(
        await until("sign-in card", CARD_SHOWN_MS, async () => {
          const cards = await this.changedEntities(
            after,
            "request",
            (value) => value?.id === id && value.status === "pending",
          );
          return { done: cards.length > 0, value: true };
        }),
      );
    } catch (error) {
      if (!(error instanceof CheckFailed)) {
        throw error;
      }
    }
    // Decline by the server id in every case, so the card never stays open.
    await this.declineCard(id);
    const declinedAfter = Date.now() - submitted;
    // The decline wakes the chat with an outcome event; archive needs it idle.
    await this.settled(agent, after);
    const card = id.slice(0, 8);
    if (!shown) {
      throw new CheckFailed(
        `card ${card} did not reach the client state in ${CARD_SHOWN_MS} ms; the smoke declined it by its server id after ${declinedAfter} ms`,
      );
    }
    if (declinedAfter > CARD_DECLINE_MS) {
      throw new CheckFailed(
        `card ${card} was declined ${declinedAfter} ms after submission; the limit is ${CARD_DECLINE_MS} ms`,
      );
    }
    return `card ${card} shown and declined ${declinedAfter} ms after submission`;
  }

  async declineCard(id: string) {
    const result = await this.backend.post<Schema["AnswerResult"]>(
      "/api/answer",
      {
        id,
        decision: "decline",
      },
    );
    if (result?.status !== "answered") {
      throw new CheckFailed(
        `the decline of card ${id.slice(0, 8)} returned ${JSON.stringify(result?.status)}`,
      );
    }
    this.openCards.delete(id);
  }

  /** Wait until the chat has no pending event after `after` and no turn in flight. */
  async settled(agent: string, after: number) {
    return until(`chat ${agent.slice(0, 8)} settles`, TURN_MS, async () => {
      const chat = await this.chatStatus(agent);
      const pending = await this.changedEntities(
        after,
        "event",
        (value) => value?.agent === agent && value.status === "pending",
      );
      return {
        done:
          chat != null &&
          !chat.inFlight &&
          TERMINAL_STATUSES.has(chat.status) &&
          pending.length === 0,
        value: chat,
        note: `status ${chat?.status}, ${pending.length} pending events`,
      };
    });
  }

  async clientTranscript(agent: string) {
    const items = await this.transcript(agent);
    const pull = await this.backend.get<Schema["SyncPullResponse"]>(
      `/api/sync/pull?scope=${encodeURIComponent(`transcript:${agent}`)}&after=0`,
    );
    const synced = syncTranscriptIds(pull?.documents ?? [], agent);
    const missing = items
      .map((item) => item.id)
      .filter((id) => !synced.includes(id));
    if (missing.length > 0) {
      throw new CheckFailed(
        `the client transcript lacks ${missing.length} items, first ${missing[0]}`,
      );
    }
    return `${items.length} items match the client transcript`;
  }

  async ownerChatsUnchanged() {
    const summary =
      await this.backend.get<Schema["UiSummaryResponse"]>("/api/ui-summary");
    const now = new Map((summary.chats ?? []).map((chat) => [chat.id, chat]));
    const changed = [...this.ownerChats].filter(([id, before]) => {
      const after = now.get(id);
      return (
        !after ||
        after.name !== before.name ||
        after.archived !== before.archived
      );
    });
    if (changed.length > 0) {
      throw new CheckFailed(
        `${changed.length} owner chats changed name or archive state, first ${changed[0]?.[0]}`,
      );
    }
    return `${this.ownerChats.size} owner chats unchanged`;
  }

  /** Stop the chat when a turn still runs, so that the archive is accepted. */
  async idle(id: string) {
    const chat = await this.chatStatus(id);
    if (!chat?.inFlight) {
      return;
    }
    await this.backend.post("/api/stop", { id });
    await until(`stop ${id.slice(0, 8)}`, STOP_MS, async () => {
      const now = await this.chatStatus(id);
      return { done: !now?.inFlight, value: now, note: now?.status };
    });
  }

  /**
   * Decline the run's open cards, archive its chats, remove its project entry
   * and delete its folder. The folder goes last: a chat woken without it fails.
   */
  async cleanup() {
    const problems = [];
    // A Map iteration skips no entry when the current entry is deleted.
    for (const [id, agent] of this.openCards) {
      try {
        const after = await this.entitySeq();
        await this.declineCard(id);
        // The decline wakes the chat; it must run while the folder exists.
        await this.settled(agent, after);
      } catch (error) {
        problems.push(
          `card ${id}: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    const archived: string[] = [];
    for (const id of this.chats) {
      try {
        await this.idle(id);
        const agent = await this.backend.post<Schema["AgentRecordResponse"]>(
          "/api/organization",
          {
            id,
            archived: true,
          },
        );
        if (agent?.archived !== true) {
          throw new CheckFailed("the server did not store the archive");
        }
        archived.push(id);
      } catch (error) {
        problems.push(
          `archive ${id}: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    try {
      // The owner sees the chat list through the client state, not the stored row.
      await until("archive in the client state", ARCHIVE_SHOWN_MS, async () => {
        const summary =
          await this.backend.get<Schema["UiSummaryResponse"]>(
            "/api/ui-summary",
          );
        const shown = new Map(
          (summary.chats ?? []).map((chat) => [chat.id, chat.archived]),
        );
        const visible = archived.filter((id) => shown.get(id) === false);
        return {
          done: visible.length === 0,
          note: `${visible.length} archived chats still show, first ${visible[0]}`,
        };
      });
    } catch (error) {
      problems.push(error instanceof Error ? error.message : String(error));
    }
    if (!this.projectRegistered && this.root) {
      try {
        const projects =
          await this.backend.get<Schema["ProjectReadResponse"]>(
            "/api/projects",
          );
        this.projectRegistered = (projects.items ?? []).some(
          (project) =>
            project.path === this.root ||
            project.locations?.some((location) => location.path === this.root),
        );
      } catch (error) {
        problems.push(
          `recover project ${this.root}: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    if (this.projectRegistered) {
      try {
        await this.backend.post("/api/projects", {
          action: "remove",
          path: this.root,
          request_id: `smoke-remove-${this.runId}`,
        });
      } catch (error) {
        problems.push(
          `project ${this.root}: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    if (this.root) {
      try {
        removeRunRoot(this.root, this.runId, this.temporaryRoot);
      } catch (error) {
        problems.push(
          `folder ${this.root}: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    if (problems.length > 0) {
      throw new CheckFailed(problems.join("; "));
    }
    return `${this.chats.length} chats archived${this.root ? `, ${this.root} removed` : ""}`;
  }

  async main(noModel: boolean | undefined) {
    const report = this.report;
    if (!(await report.run("preflight", () => this.preflight()))) {
      return report;
    }
    if (!(await report.run("installed build", () => this.build())))
      return report;
    if (!(await report.run("served renderer", () => this.renderer())))
      return report;
    if (!(await report.run("ready logins", () => this.accounts()))) {
      return report;
    }
    if (noModel) {
      report.skip("scenarios", "--no-model");
      return report;
    }
    if (!(await report.run("session", () => this.session()))) {
      return report;
    }
    const stop = () => {
      this.interrupted = true;
      this.cancel.abort(new CheckFailed("smoke interrupted"));
    };
    process.on("SIGINT", stop);
    process.on("SIGTERM", stop);
    try {
      this.makeRoot();
      if (this.usage) this.saveUsage();
      let first = null;
      for (const provider of this.accountsByProvider.keys()) {
        const current: { chat?: Chat } = {};
        const passed = await report.run(`${provider} turn`, async () => {
          current.chat = await this.turn(provider);
          if (this.usage)
            return await this.usageScenario(current.chat, provider);
          return current.chat.detail;
        });
        if (this.live) {
          await this.live.close();
          this.live = null;
        }
        if (passed && !first && current.chat) {
          first = current.chat.id;
        }
        if (this.interrupted) {
          break;
        }
      }
      if (first && !this.interrupted && !this.usage) {
        const protocol =
          await this.backend.get<Schema["SyncProtocolResponse"]>(
            "/api/sync/protocol",
          );
        const live = new LiveChanges(
          this.backend,
          first,
          protocol.protocolVersion,
        );
        this.live = live;
        await live.ready;
        await report.run("message once", () => this.messageOnce(first));
        await report.run("monitor exit", () => this.monitor(first));
        await report.run("sign-in card", () => this.signInCard(first));
        await report.run("client transcript", () =>
          this.clientTranscript(first),
        );
      } else if (!first) {
        report.skip("chat scenarios", "no provider turn passed");
      }
    } finally {
      this.backend.signal = undefined;
      try {
        await report.run("cleanup", () => this.cleanup());
        await report.run("owner chats", () => this.ownerChatsUnchanged());
        if (this.live) {
          await this.live.close();
          this.live = null;
        }
        if (this.usage) {
          this.saveUsage();
          console.table(usageTable(this.usageEvidence, "RUN"));
          console.log(`Usage evidence: ${this.evidencePath}`);
        }
      } finally {
        process.removeListener("SIGINT", stop);
        process.removeListener("SIGTERM", stop);
      }
    }
    return report;
  }
}

function options(argv: string[]) {
  const { values } = parseArgs({
    args: argv,
    options: {
      app: { type: "string", default: DEFAULT_APP },
      url: { type: "string", default: DEFAULT_URL },
      provider: { type: "string", multiple: true },
      model: { type: "string", multiple: true },
      "no-model": { type: "boolean", default: false },
      usage: { type: "boolean", default: false },
      collect: { type: "string" },
      "sqlite-ro-agent": { type: "string" },
      compare: { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
  });
  const model: Record<string, string> = {};
  for (const entry of values.model ?? []) {
    const [provider, id] = entry.split("=");
    if (!provider || !id) {
      throw new CheckFailed(`--model takes provider=model, not ${entry}`);
    }
    model[provider] = id;
  }
  return { ...values, model, providers: values.provider };
}

const usage = `Usage: just smoke-installed [--provider codex|claude]... [--model provider=id]... [--no-model] [--usage] [--app path] [--url url]
       just smoke-installed --compare <old.json> <new.json>
       just smoke-installed --collect <run.json>
Checks the installed application on Linux or macOS with its own test chats; see docs/testing.md, "Installed scenario smoke".
`;

async function cli() {
  if (process.argv[2] === "--compare") {
    const files = process.argv.slice(3);
    if (files.length !== 2)
      throw new CheckFailed("--compare requires <old.json> <new.json>");
    console.table(
      compareEvidence(
        readEvidence(files[0] ?? ""),
        readEvidence(files[1] ?? ""),
      ),
    );
    return 0;
  }
  const parsed = options(process.argv.slice(2));
  if (parsed.help) {
    process.stdout.write(usage);
    return 0;
  }
  if (parsed.usage && (parsed.providers || Object.keys(parsed.model).length))
    throw new CheckFailed(
      "--usage always uses both providers and their cheapest offered models",
    );
  const smoke = new Smoke(parsed);
  process.stdout.write(
    `Studio scenario smoke ${smoke.runId}: ${parsed.app} at ${parsed.url}\n`,
  );
  if (parsed["sqlite-ro-agent"] && !parsed.collect)
    throw new CheckFailed(
      "--sqlite-ro-agent is only for explicitly approved archived-run recovery",
    );
  if (parsed.collect) {
    await smoke.recover(parsed.collect);
    return 0;
  }
  const report = await smoke.main(parsed["no-model"]);
  report.print();
  return report.failed().length === 0 ? 0 : 1;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(fs.realpathSync(process.argv[1])).href
) {
  cli().then(
    (code) => process.exit(code),
    (error) => {
      process.stderr.write(
        `${error instanceof CheckFailed ? error.message : error.stack}\n`,
      );
      process.exit(2);
    },
  );
}
