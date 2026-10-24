#!/usr/bin/env node
// Scenario smoke of the Codex Studio app installed on this Mac.
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

const DEFAULT_APP = "/Applications/Codex Studio.app";
const DEFAULT_URL = "http://127.0.0.1:4620";
/** Prefix of the run folder below the platform temporary directory. */
export const RUN_PREFIX = "studio-smoke-";
/** Marker file in the run folder; it holds the run id. */
export const RUN_MARKER = "studio-smoke-run.json";
/** Seconds a backend must have run before the smoke uses it. */
const BACKEND_SETTLED_SECONDS = 60;
/** Milliseconds for one HTTP request. */
const REQUEST_MS = 30_000;
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
/** models.dev provider names of the Studio providers. */
const PRICING_PROVIDERS = { codex: "openai", claude: "anthropic" };
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
  constructor() {
    this.rows = [];
  }

  async run(name, check) {
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
          : String(error?.stack ?? error);
      this.rows.push({
        name,
        status: "FAIL",
        detail,
        ms: Date.now() - started,
      });
      return false;
    }
  }

  skip(name, reason) {
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
export function elapsedSeconds(value) {
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
export function cheapestModel(models, pricing, provider) {
  const priced =
    pricing?.catalog?.providers?.[PRICING_PROVIDERS[provider]]?.models ?? {};
  let best = null;
  for (const row of models) {
    if (row?.hidden || typeof row?.model !== "string") {
      continue;
    }
    const cost = priced[row.model]?.cost ?? priced[row.resolvedModel]?.cost;
    if (typeof cost?.input !== "number" || typeof cost?.output !== "number") {
      continue;
    }
    const price = cost.input + cost.output;
    if (!best || price < best.price) {
      best = {
        model: row.model,
        price,
        efforts: row.supportedReasoningEfforts ?? [],
      };
    }
  }
  return best;
}

/** The lowest reasoning effort that a model row lists, or undefined. */
export function lowestEffort(efforts) {
  const order = ["minimal", "low", "medium", "high"];
  const names = efforts.map((entry) =>
    typeof entry === "string" ? entry : entry?.reasoningEffort,
  );
  return order.find((name) => names.includes(name));
}

/** Transcript items that carry the user message `id`. */
export function messageItems(items, id) {
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
export function syncTranscriptIds(documents, agent) {
  const head = documents.find(
    (document) => document.id === `transcript:${agent}` && !document._deleted,
  );
  if (!head) {
    throw new CheckFailed(
      `the transcript pull has no document transcript:${agent}`,
    );
  }
  const payload = JSON.parse(head.payload);
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
export function browserCardId(bridgeId) {
  const namespace = Buffer.from(URL_NAMESPACE.replaceAll("-", ""), "hex");
  const bytes = createHash("sha1")
    .update(namespace)
    .update(`studio-browser-open:${bridgeId}`)
    .digest()
    .subarray(0, 16);
  bytes[6] = (bytes[6] & 0x0f) | 0x50;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
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
export function removeRunRoot(root, runId, temporaryRoot) {
  const resolved = fs.realpathSync(root);
  const parent = fs.realpathSync(temporaryRoot);
  if (
    path.dirname(resolved) !== parent ||
    !path.basename(resolved).startsWith(RUN_PREFIX)
  ) {
    throw new CheckFailed(`${root} is not a run folder below ${parent}`);
  }
  const marker = JSON.parse(
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

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function until(label, ms, probe) {
  const deadline = Date.now() + ms;
  let last;
  while (Date.now() < deadline) {
    last = await probe();
    if (last?.done) {
      return last.value;
    }
    await sleep(POLL_MS);
  }
  throw new CheckFailed(
    `${label}: timed out after ${Math.round(ms / 1000)} s${last?.note ? ` (${last.note})` : ""}`,
  );
}

function sha256File(file) {
  return createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function command(file, args) {
  return execFileSync(file, args, {
    encoding: "utf8",
    timeout: 10_000,
    stdio: ["ignore", "pipe", "ignore"],
  });
}

/** PIDs of processes that have `file` open, by lsof; empty when none. */
function lockHolders(file) {
  if (!fs.existsSync(file)) {
    return [];
  }
  try {
    return command("/usr/sbin/lsof", ["-t", file])
      .split(/\s+/)
      .filter(Boolean)
      .map(Number);
  } catch (error) {
    // lsof exits 1 when no process has the file open.
    if (error.status === 1) {
      return [];
    }
    throw error;
  }
}

/** The live backend over HTTP, as the renderer reaches it from the loopback origin. */
class Backend {
  constructor(url) {
    this.url = url.replace(/\/$/, "");
    this.token = "";
  }

  async request(method, route, body, ms = REQUEST_MS) {
    const headers = { accept: "application/json" };
    if (body !== undefined) {
      headers["content-type"] = "application/json";
    }
    if (method !== "GET" && method !== "HEAD") {
      if (!this.token) {
        throw new CheckFailed(`no session token for ${method} ${route}`);
      }
      headers["x-canvas-token"] = this.token;
    }
    const init = {
      method,
      headers,
      redirect: "manual",
      signal: AbortSignal.timeout(ms),
    };
    if (body !== undefined) {
      init.body = JSON.stringify(body);
    }
    const response = await fetch(this.url + route, init);
    const text = await response.text();
    if (!response.ok) {
      throw new CheckFailed(
        `${method} ${route}: HTTP ${response.status} ${text.slice(0, 300)}`,
      );
    }
    return text ? JSON.parse(text) : null;
  }

  get(route, ms) {
    return this.request("GET", route, undefined, ms);
  }

  post(route, body, ms) {
    return this.request("POST", route, body, ms);
  }
}

class Smoke {
  constructor(options) {
    this.app = options.app;
    this.backend = new Backend(options.url);
    this.model = options.model;
    this.providers = options.providers;
    this.report = new Report();
    this.runId = randomBytes(4).toString("hex");
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

  workspace() {
    return path.join(this.app, "Contents", "Resources", "workspace");
  }

  async preflight() {
    const desktop = await this.backend.get("/api/desktop");
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
      .filter(Boolean)
      .map(([, pid, line]) => ({ pid: Number(pid), line }));
    // A second backend of this bundle is starting, or waits for the runtime lock.
    const server = path.join(this.workspace(), "bin", "studio-runtime-server");
    const others = processes.filter(
      (row) => row.line.startsWith(server) && row.pid !== desktop.pid,
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
    const summary = await this.backend.get("/api/ui-summary");
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
    const server = path.join(this.workspace(), "bin", "studio-runtime-server");
    const running = command("/bin/ps", [
      "-p",
      String(this.desktop.pid),
      "-o",
      "comm=",
    ]).trim();
    if (path.resolve(running) !== path.resolve(server)) {
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
    const served = await this.backend.get("/studio-renderer.json");
    const installed = JSON.parse(
      fs.readFileSync(
        path.join(this.workspace(), "web", "dist", "studio-renderer.json"),
        "utf8",
      ),
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
    const listing = await this.backend.get("/api/accounts");
    const ready = (listing.accounts ?? []).filter(
      (account) =>
        account.status === "ready" && !account.disconnected && !account.deleted,
    );
    const chosen = new Map();
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
    this.accountsByProvider = chosen;
    return [...chosen]
      .map(([provider, account]) => `${provider}: ${account}`)
      .join(", ");
  }

  async session() {
    const session = await this.backend.get("/api/session");
    if (typeof session?.token !== "string" || !session.token) {
      throw new CheckFailed("the backend gave no session token");
    }
    this.backend.token = session.token;
    return "session token read";
  }

  makeRoot() {
    this.root = fs.mkdtempSync(path.join(this.temporaryRoot, RUN_PREFIX));
    fs.writeFileSync(
      path.join(this.root, RUN_MARKER),
      JSON.stringify({
        runId: this.runId,
        pid: process.pid,
        created: new Date().toISOString(),
      }),
    );
    return this.root;
  }

  async chooseModel(provider, account) {
    if (this.model?.[provider]) {
      return { model: this.model[provider], efforts: [] };
    }
    const pricing = JSON.parse(
      fs.readFileSync(path.join(this.state, PRICING_ARTIFACT), "utf8"),
    );
    const listing = await this.backend.get(
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

  async transcript(agent) {
    const page = await this.backend.get(
      `/api/transcript/page?id=${encodeURIComponent(agent)}&limit=200`,
    );
    return page?.items ?? [];
  }

  async chatStatus(agent) {
    const summary = await this.backend.get("/api/ui-summary");
    return (summary.chats ?? []).find((chat) => chat.id === agent);
  }

  /** Wait until the chat's turn ends and an assistant item contains `token`. */
  async answered(agent, token, ms) {
    return until(`answer ${token}`, ms, async () => {
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
    });
  }

  async turn(provider) {
    const account = this.accountsByProvider.get(provider);
    const { model, efforts } = await this.chooseModel(provider, account);
    const id = randomUUID();
    const token = `SMOKE_${this.runId}_${provider.toUpperCase()}`;
    // Archive this chat even when the create response is lost.
    this.chats.push(id);
    const effort = lowestEffort(efforts);
    await this.backend.post("/api/agents", {
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
    this.projectRegistered = true;
    await this.answered(id, token, TURN_MS);
    return {
      id,
      detail: `${model}${effort ? ` ${effort}` : ""} answered on ${account}`,
    };
  }

  async messageOnce(agent) {
    const id = randomUUID();
    const token = `SMOKE_${this.runId}_MESSAGE`;
    const message = {
      id,
      room: agent,
      text: `Reply with exactly ${token}. Do not use tools.`,
      delivery: "next_turn",
    };
    await this.backend.post("/api/messages", message);
    // An exact retry of the same id must not enqueue the message again.
    await this.backend.post("/api/messages", message);
    const items = await this.answered(agent, token, TURN_MS);
    const copies = messageItems(items, id);
    if (copies.length !== 1) {
      throw new CheckFailed(
        `the transcript has ${copies.length} copies of message ${id}`,
      );
    }
    const receipts = await this.backend.get(
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
    const page = await this.backend.get(
      "/api/sync/pull?scope=state:entities:v1&after=0&limit=1",
    );
    return page?.maxSeq ?? 0;
  }

  /** Entity documents of `collection` changed after `after` whose value matches `accept`. */
  async changedEntities(after, collection, accept) {
    const found = [];
    let cursor = after;
    for (;;) {
      const page = await this.backend.get(
        `/api/sync/pull?scope=state:entities:v1&after=${cursor}`,
      );
      for (const document of page?.documents ?? []) {
        if (
          !document.id?.startsWith(`entity:${collection}:`) ||
          document._deleted
        ) {
          continue;
        }
        const payload = JSON.parse(document.payload);
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

  async monitor(agent) {
    const after = await this.entitySeq();
    const token = `SMOKE_${this.runId}_MONITOR`;
    await this.backend.post("/api/messages", {
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
    return `monitor ${finished[0].id ?? "?"} completed with exit 0`;
  }

  /** Submit a sign-in card the way a command's $BROWSER does, then decline it at once. */
  async signInCard(agent) {
    const socket = path.join(this.state, "browser-host", "open.sock");
    const after = await this.entitySeq();
    const submitted = Date.now();
    const reply = await new Promise((resolve, reject) => {
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
      shown = await until("sign-in card", CARD_SHOWN_MS, async () => {
        const cards = await this.changedEntities(
          after,
          "request",
          (value) => value?.id === id && value.status === "pending",
        );
        return { done: cards.length > 0, value: true };
      });
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

  async declineCard(id) {
    const result = await this.backend.post("/api/answer", {
      id,
      decision: "decline",
    });
    if (result?.status !== "answered") {
      throw new CheckFailed(
        `the decline of card ${id.slice(0, 8)} returned ${JSON.stringify(result?.status)}`,
      );
    }
    this.openCards.delete(id);
  }

  /** Wait until the chat has no pending event after `after` and no turn in flight. */
  async settled(agent, after) {
    return until(`chat ${agent.slice(0, 8)} settles`, TURN_MS, async () => {
      const chat = await this.chatStatus(agent);
      const pending = await this.changedEntities(
        after,
        "event",
        (value) => value?.agent === agent && value.status === "pending",
      );
      return {
        done:
          Boolean(chat) &&
          !chat.inFlight &&
          TERMINAL_STATUSES.has(chat.status) &&
          pending.length === 0,
        value: chat,
        note: `status ${chat?.status}, ${pending.length} pending events`,
      };
    });
  }

  async clientTranscript(agent) {
    const items = await this.transcript(agent);
    const pull = await this.backend.get(
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
    const summary = await this.backend.get("/api/ui-summary");
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
        `${changed.length} owner chats changed name or archive state, first ${changed[0][0]}`,
      );
    }
    return `${this.ownerChats.size} owner chats unchanged`;
  }

  /** Stop the chat when a turn still runs, so that the archive is accepted. */
  async idle(id) {
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
        problems.push(`card ${id}: ${error.message}`);
      }
    }
    const archived = [];
    for (const id of this.chats) {
      try {
        await this.idle(id);
        const agent = await this.backend.post("/api/organization", {
          id,
          archived: true,
        });
        if (agent?.archived !== true) {
          throw new CheckFailed("the server did not store the archive");
        }
        archived.push(id);
      } catch (error) {
        problems.push(`archive ${id}: ${error.message}`);
      }
    }
    try {
      // The owner sees the chat list through the client state, not the stored row.
      await until("archive in the client state", ARCHIVE_SHOWN_MS, async () => {
        const summary = await this.backend.get("/api/ui-summary");
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
      problems.push(error.message);
    }
    if (this.projectRegistered) {
      try {
        await this.backend.post("/api/projects", {
          action: "remove",
          path: this.root,
          request_id: `smoke-remove-${this.runId}`,
        });
      } catch (error) {
        problems.push(`project ${this.root}: ${error.message}`);
      }
    }
    if (this.root) {
      try {
        removeRunRoot(this.root, this.runId, this.temporaryRoot);
      } catch (error) {
        problems.push(`folder ${this.root}: ${error.message}`);
      }
    }
    if (problems.length > 0) {
      throw new CheckFailed(problems.join("; "));
    }
    return `${this.chats.length} chats archived${this.root ? `, ${this.root} removed` : ""}`;
  }

  async main(noModel) {
    const report = this.report;
    if (!(await report.run("preflight", () => this.preflight()))) {
      return report;
    }
    await report.run("installed build", () => this.build());
    await report.run("served renderer", () => this.renderer());
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
    };
    process.once("SIGINT", stop);
    process.once("SIGTERM", stop);
    try {
      this.makeRoot();
      let first = null;
      for (const provider of this.accountsByProvider.keys()) {
        let chat = null;
        const passed = await report.run(`${provider} turn`, async () => {
          chat = await this.turn(provider);
          return chat.detail;
        });
        if (passed && !first) {
          first = chat.id;
        }
        if (this.interrupted) {
          break;
        }
      }
      if (first && !this.interrupted) {
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
      await report.run("cleanup", () => this.cleanup());
      await report.run("owner chats", () => this.ownerChatsUnchanged());
    }
    return report;
  }
}

function options(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      app: { type: "string", default: DEFAULT_APP },
      url: { type: "string", default: DEFAULT_URL },
      provider: { type: "string", multiple: true },
      model: { type: "string", multiple: true },
      "no-model": { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
  });
  const model = {};
  for (const entry of values.model ?? []) {
    const [provider, id] = entry.split("=");
    if (!provider || !id) {
      throw new CheckFailed(`--model takes provider=model, not ${entry}`);
    }
    model[provider] = id;
  }
  return { ...values, model, providers: values.provider };
}

const usage = `Usage: just smoke-installed [--provider codex|claude]... [--model provider=id]... [--no-model] [--app path] [--url url]
Checks the installed app on this Mac with its own test chats; see docs/testing.md, "Installed scenario smoke".
`;

async function cli() {
  const parsed = options(process.argv.slice(2));
  if (parsed.help) {
    process.stdout.write(usage);
    return 0;
  }
  const smoke = new Smoke(parsed);
  process.stdout.write(
    `Studio scenario smoke ${smoke.runId}: ${parsed.app} at ${parsed.url}\n`,
  );
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
