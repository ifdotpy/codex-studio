import assert from "node:assert/strict";
import { assertStoredScaleProfile } from "./scaleVerifierHelpers.ts";
import { auditSummaryLines } from "./uiTeamScaleAudit.mjs";

// Helpers of verify-ui-team-scale.mjs. Every value below has one owner:
// this module for the verifier's own pacing and limits, the runtime for the
// spawn batch size (`orchestration_spawn`, docs/orchestration.md).

// Sampling period of the live observation loop, milliseconds.
export const SAMPLE_INTERVAL_MS = 1_000;
// Period of /api/diagnostics snapshots during a run, milliseconds.
export const DIAGNOSTICS_INTERVAL_MS = 5_000;
// Period of the typed-character echo probe, milliseconds.
export const ECHO_PROBE_INTERVAL_MS = 3_000;
// Frame gaps shorter than this are not kept in the evidence, milliseconds.
export const FRAME_GAP_RECORD_MS = 100;
// Largest worker batch of one `orchestration_spawn` call (runtime contract).
export const SPAWN_BATCH_MAX = 64;
// Largest team this verifier accepts (workers, excluding the lead).
export const WORKERS_MAX = 512;
// Seconds the worker's shell command sleeps before it prints its token.
export const WORKER_COMMAND_SECONDS = 3;
// Sampling requests the provider admits at once per account before it answers
// 429 (assumed from a live Codex run: about 10), requests.
export const ACCOUNT_CONCURRENT_REQUESTS = 10;
// Length of one provider retry wave after a 429 (observed 51-60 s), milliseconds.
export const THROTTLE_WAVE_MS = 60_000;
// Stage deadline from sending the prompt: a fixed part, a part per worker, and
// one throttle wave for every `ACCOUNT_CONCURRENT_REQUESTS` workers per account.
export const STAGE_DEADLINE_BASE_MS = 120_000;
export const STAGE_DEADLINE_PER_WORKER_MS = 500;
// Wait for the cleanup to be observed (rows deleted, processes gone).
export const CLEANUP_DEADLINE_MS = 120_000;
// Pass limits.
export const FRAME_GAP_LIMIT_MS = 1_000;
export const ECHO_LATENCY_LIMIT_MS = 300;
export const API_P95_LIMIT_MS = 1_000;
// One fully busy core is 100 percent.
const PERCENT = 100;

export const LEAD_PROFILE_NAMES = Object.freeze([
  "codex-luna",
  "claude-sonnet",
]);
export const WORKER_PROFILE_NAMES = Object.freeze([
  "codex-luna",
  "claude-haiku",
]);

// Consume current resources and multipart entity-transfer frames. The server's
// sync protocol and renderer resourceEvents.ts own these wire envelopes.
export function createEntityFrameReader(accept) {
  const transfers = new Map();
  const receive = (name, frame) => {
    if (name === "resources") {
      for (const version of frame.resourceVersions) {
        if (version.entityChanges) accept(version.entityChanges.documents);
      }
    } else if (name === "entity-transfer") {
      const transfer = transfers.get(frame.transferId) || {
        pageIndex: 0,
        parts: [],
        documents: [],
        through: null,
      };
      transfers.set(frame.transferId, transfer);
      assert.equal(
        frame.pageIndex,
        transfer.pageIndex,
        "Entity transfer pages arrived out of order.",
      );
      assert.equal(
        frame.partIndex,
        transfer.parts.length,
        "Entity transfer parts arrived out of order.",
      );
      if (!transfer.parts.length) transfer.partCount = frame.partCount;
      assert.equal(
        frame.partCount,
        transfer.partCount,
        "Entity transfer part count changed.",
      );
      assert.ok(
        !frame.complete || frame.partIndex + 1 === frame.partCount,
        "Entity transfer completed before its last part.",
      );
      transfer.parts.push(frame.data);
      if (transfer.parts.length === frame.partCount) {
        const page = JSON.parse(transfer.parts.join(""));
        if (transfer.through !== null)
          assert.equal(
            page.after,
            transfer.through,
            "Entity transfer cursor interval is discontinuous.",
          );
        transfer.through = page.through;
        transfer.documents.push(...page.documents);
        transfer.parts = [];
        transfer.pageIndex += 1;
        if (frame.complete) {
          transfers.delete(frame.transferId);
          accept(transfer.documents);
        }
      }
    } else if (name === "api-schema" && frame.mismatch) {
      throw new Error("Sync API schema mismatch.");
    }
  };
  receive.assertComplete = () =>
    assert.equal(
      transfers.size,
      0,
      "Required entity transfer is incomplete; observation is inconclusive.",
    );
  return receive;
}

// A bounded connection attempt, then an explicitly owned SSE reader. Closing
// aborts its fetch and waits for the reader before browser teardown returns.
export async function openEntityStream({
  origin,
  token,
  protocol,
  resource,
  timeoutMs,
  receive,
  failed,
}) {
  const controller = new AbortController();
  const timer = setTimeout(
    () => controller.abort(new Error("Sync connection timed out.")),
    timeoutMs,
  );
  const url = new URL(protocol.streamEndpoint, origin);
  url.searchParams.set("protocol", String(protocol.protocolVersion));
  url.searchParams.set("resources", JSON.stringify([resource]));
  url.searchParams.set("tokenRates", "false");
  let response;
  try {
    response = await fetch(url, {
      headers: { origin, "x-canvas-token": token },
      signal: controller.signal,
    });
    assert.ok(
      response.ok && response.body,
      `Sync stream returned HTTP ${response.status}.`,
    );
  } catch (error) {
    controller.abort();
    throw error;
  } finally {
    clearTimeout(timer);
  }
  const done = (async () => {
    const decoder = new TextDecoder();
    let buffer = "";
    for await (const chunk of response.body) {
      buffer += decoder
        .decode(chunk, { stream: true })
        .replaceAll("\r\n", "\n");
      let end;
      while ((end = buffer.indexOf("\n\n")) >= 0) {
        const block = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        const lines = block.split("\n");
        const name = lines
          .find((line) => line.startsWith("event:"))
          ?.slice(6)
          .trim();
        const data = lines
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).trimStart())
          .join("\n");
        if (name && data) receive(name, JSON.parse(data));
      }
    }
    if (!controller.signal.aborted)
      throw new Error("Sync stream ended before teardown.");
  })().catch((error) => {
    if (!controller.signal.aborted) failed(error);
  });
  return {
    close: async () => {
      controller.abort();
      await done;
    },
  };
}

// Global state publishes agentSummary; open-chat fields come from each owned
// agentDetail subscription. Merge them like entityProjection.ts, with summary
// identity/status fields authoritative. No server data is polled.
export function createEntityReader(subscribe, onAgent, onEvent = () => {}) {
  const summaries = new Map();
  const details = new Map();
  const streams = new Map();
  const pending = new Set();
  const frameReaders = new Set();
  let rootId = null;
  let failure = null;
  let closed = false;
  const owned = (value) =>
    rootId !== null && (value.id === rootId || value.rootId === rootId);
  const emit = (id) => {
    const summary = summaries.get(id);
    if (summary && owned(summary))
      onAgent({ ...details.get(id), ...summary, deleted: false });
  };
  const start = (resource) => {
    const key = JSON.stringify(resource);
    if (closed || streams.has(key)) return;
    const receive = createEntityFrameReader((documents) => {
      for (const entity of documents) {
        if (entity.collection === "event") {
          onEvent(
            entity.op === "delete"
              ? { id: entity.id, deleted: true }
              : entity.value,
          );
        } else if (entity.collection === "agentSummary") {
          if (entity.op === "delete") {
            const previous = summaries.get(entity.id);
            summaries.delete(entity.id);
            if (previous && owned(previous))
              onAgent({ ...previous, deletedAt: true, deleted: true });
          } else {
            assert.equal(entity.op, "upsert");
            summaries.set(entity.id, entity.value);
            if (rootId && owned(entity.value))
              start({ kind: "agentDetail", agentId: entity.id });
            emit(entity.id);
          }
        } else if (entity.collection === "agentDetail") {
          if (entity.op === "delete") details.delete(entity.id);
          else {
            assert.equal(entity.op, "upsert");
            details.set(entity.id, entity.value);
          }
          emit(entity.id);
        }
      }
    });
    frameReaders.add(receive);
    const opening = Promise.resolve().then(() =>
      subscribe(resource, receive, (error) => {
        failure ??= error;
      }),
    );
    streams.set(key, opening);
    pending.add(opening);
    void opening
      .catch((error) => {
        failure ??= error;
      })
      .finally(() => pending.delete(opening));
  };
  const pull = async () => {
    start({ kind: "state" });
    await Promise.allSettled(pending);
    if (failure) throw failure;
  };
  pull.assertComplete = () => {
    if (failure) throw failure;
    for (const reader of frameReaders) reader.assertComplete();
  };
  pull.watch = (id) => {
    rootId = id;
    start({ kind: "agentDetail", agentId: id });
    for (const summary of summaries.values())
      if (owned(summary)) {
        start({ kind: "agentDetail", agentId: summary.id });
        emit(summary.id);
      }
  };
  pull.close = async () => {
    closed = true;
    await Promise.all(
      [...streams.values()].map(async (opening) => {
        const stream = await opening.catch(() => null);
        await stream?.close();
      }),
    );
  };
  return pull;
}

// Cleanup can establish agent/process absence, but diagnostics has no thread IDs.
export const OWNED_CLEANUP_VERIFIED_STATE =
  "agents and owned processes verified; owned loaded threads not verifiable (diagnostics expose counts only)";

// Global provider counts cannot identify a leak on a shared backend.
export function cleanupVerified({
  remaining,
  leakedProviderPids,
  leakedProcesses,
}) {
  return (
    remaining === 0 &&
    leakedProviderPids.length === 0 &&
    leakedProcesses.length === 0
  );
}

// Serialized into headless Chromium by page.evaluate; keep this self-contained.
export function readTeamUiCounts(document = globalThis.document) {
  const completedCollapsed = !!document.querySelector(
    '#team .worker-group [aria-expanded="false"]',
  );
  return {
    teamCount: Number(
      document.querySelector('#team [data-team-count="all"] strong')
        ?.textContent || 0,
    ),
    workerCards:
      document.querySelectorAll("#team [data-worker]").length +
      (completedCollapsed
        ? Number(
            document.querySelector('#team [data-team-count="completed"] strong')
              ?.textContent || 0,
          )
        : 0),
    sidebarChats: document.querySelectorAll("#sidebar [data-chat]").length,
    teamPanelClosed:
      document.querySelector("#team-toggle")?.getAttribute("aria-expanded") ===
      "false",
  };
}

// The New chat draft is an inline role picker. Keep every action inside its
// dialog: Escape would close the whole draft, and settings popovers belong to
// existing chats. Radio labels are clicked because native inputs are styled.
async function chooseSegment(dialog, label, name) {
  const group = dialog.getByRole("radiogroup", { name: label, exact: true });
  const radio = group.getByRole("radio", { name, exact: true });
  const id = await radio.getAttribute("id");
  await group.locator(`label[for="${id}"]`).click();
  await group
    .getByRole("radio", { name, exact: true, checked: true })
    .waitFor({ state: "attached" });
}

// Alias deduplication can retain a recommended default instead of the short
// alias. Accept it only when the authoritative catalog resolves it to the
// same approved model; never select an account's unverified default.
export function approvedModelUiValues(catalog, candidate) {
  const identity = candidate.resolvedModel ?? candidate.model;
  return [
    ...new Set(
      catalog.data
        .filter((row) => (row.resolvedModel ?? row.model) === identity)
        .map((row) => row.model),
    ),
  ];
}

export function approvedStoredModel(value, role) {
  assert.ok(
    value === role.model || role.uiModelValues.includes(value),
    `Stored model ${value} does not resolve to the approved ${role.model}.`,
  );
  return role.model;
}

// Shared by the actual post-create path and its regression witness. The
// backend stores aliases; all other profile fields remain exact assertions.
export function assertCreatedTeamProfiles(entity, lead, worker) {
  assertStoredScaleProfile(
    entity
      ? { ...entity, model: approvedStoredModel(entity.model, lead) }
      : entity,
    {
      id: entity?.id,
      model: lead.model,
      effort: lead.effort,
      fastMode: lead.profile.fastMode,
    },
  );
  assert.equal(
    entity.accountKey,
    lead.accountKey,
    "The lead is not on the selected account.",
  );
  assert.equal(
    approvedStoredModel(entity.workerDefaults?.model, worker),
    worker.model,
    "Worker default model differs from the approved model.",
  );
  assert.equal(
    entity.workerDefaults?.effort,
    worker.effort,
    "Worker default effort differs from the approved effort.",
  );
}

export async function configureNewChat(dialog, lead, worker) {
  for (const [role, prefix, accountLabel, settings] of [
    ["Lead", "Main agent", "Main agent account", lead],
    ["Workers", "Default subagent", "Subagent account", worker],
  ]) {
    await chooseSegment(dialog, "Agent role", role);
    await chooseSegment(
      dialog,
      `${accountLabel} provider`,
      settings.profile.provider === "claude" ? "Claude" : "Codex",
    );
    const accounts = dialog.getByRole("group", {
      name: accountLabel,
      exact: true,
    });
    // A sole selected lead account is rendered as read-only status, not a button.
    if ((await accounts.getAttribute("data-value")) !== settings.accountKey) {
      await accounts
        .getByRole("button", { name: /^Select account / })
        .and(dialog.locator(`[data-account-key="${settings.accountKey}"]`))
        .click();
    }
    const models = dialog.getByRole("listbox", {
      name: `${prefix} model`,
      exact: true,
    });
    const values = settings.uiModelValues ?? [settings.uiModel];
    const model = dialog.locator(
      values.map((value) => `[data-value="${value}"]`).join(", "),
    );
    await models.getByRole("option").and(model).first().click();
    await models
      .getByRole("option", { selected: true })
      .and(model)
      .first()
      .waitFor();
    await chooseSegment(
      dialog,
      `${prefix} reasoning`,
      settings.effort[0].toUpperCase() + settings.effort.slice(1),
    );
    const fast = dialog.getByRole("switch", { name: "Fast mode", exact: true });
    if (await fast.count()) await fast.setChecked(settings.profile.fastMode);
  }
}

export function stageDeadlineMs(workers, accounts = 1) {
  const waves = Math.ceil(workers / (ACCOUNT_CONCURRENT_REQUESTS * accounts));
  return (
    STAGE_DEADLINE_BASE_MS +
    workers * STAGE_DEADLINE_PER_WORKER_MS +
    waves * THROTTLE_WAVE_MS
  );
}

export function percentile(values, fraction) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[
    Math.min(sorted.length - 1, Math.ceil(fraction * sorted.length) - 1)
  ];
}

export function workerToken(runTag, index) {
  return `${runTag}_W${String(index).padStart(4, "0")}`;
}

export function leadDoneToken(runTag) {
  return `${runTag}_ALL_REPLIED`;
}

// One prompt, typed like a user would. The lead gets the token rule instead of
// a list so that the prompt stays short; it spawns the workers in parallel
// batches inside one turn.
// The spawn calls of a team: call `call` creates workers `from`..`to` (1-based).
export function spawnCalls(workers) {
  return Array.from(
    { length: Math.ceil(workers / SPAWN_BATCH_MAX) },
    (_, i) => ({
      call: i + 1,
      from: i * SPAWN_BATCH_MAX + 1,
      to: Math.min(workers, (i + 1) * SPAWN_BATCH_MAX),
    }),
  );
}

export function buildLeadPrompt({ workers, runTag, workspace }) {
  const calls = spawnCalls(workers);
  const plan = calls
    .map(({ call, from, to }) => `call ${call}: agents ${from} to ${to}`)
    .join("; ");
  return [
    `Automated Studio acceptance test ${runTag}. Do not edit files.`,
    `Spawn exactly ${workers} subagents with orchestration_spawn, numbered 1 to ${workers}.`,
    `Use exactly ${calls.length} call(s), one per range (${plan}); issue all calls in this turn without waiting between them, and repeat any call that failed.`,
    `Give every agent workspace "${workspace}", the default model and effort, and this task with N replaced by its number zero-padded to 4 digits:`,
    `"Run exactly one shell command: sleep ${WORKER_COMMAND_SECONDS}; echo ${runTag}_WNNNN. Then reply with only the printed token and stop."`,
    `Do not send messages to the agents; wait for their results.`,
    `Spawn nothing else and run no commands yourself. Do not answer until all ${workers} agents have replied; then answer with only ${leadDoneToken(runTag)}.`,
  ].join(" ");
}

// The agent number at the end of a worker name: the lead chooses names such
// as "7" or "acceptance-0007"; null when the name ends in no number.
function workerNumber(name) {
  const digits = /(\d+)\s*$/.exec(name ?? "");
  return digits ? Number(digits[1]) : null;
}

// Per spawn call (agents numbered by their names): how many workers exist and
// when the first and last appeared, started and replied (milliseconds since the
// prompt, null when not observed).
export function spawnBatchTimings(rows, workers) {
  const earliest = (values) => (values.length ? Math.min(...values) : null);
  const latest = (values) => (values.length ? Math.max(...values) : null);
  return spawnCalls(workers).map(({ call, from, to }) => {
    const members = rows.filter((row) => {
      const number = workerNumber(row.name);
      return number !== null && number >= from && number <= to;
    });
    const seen = (key) =>
      members.map((row) => row[key]).filter((value) => value !== null);
    return {
      call,
      from,
      to,
      created: members.length,
      firstSeenAtMs: earliest(seen("firstSeenAtMs")),
      lastSeenAtMs: latest(seen("firstSeenAtMs")),
      firstStartedAtMs: earliest(seen("startedAtMs")),
      lastCompletedAtMs: latest(seen("completedAtMs")),
      replied: members.filter((row) => row.completedAtMs !== null).length,
    };
  });
}

// Tracks the agents of one team from entity documents. `observe` takes the
// stored agent value and the sample time (milliseconds since the prompt).
export function createTeamTracker(leadId, runTag, workers) {
  const expected = new Map();
  for (let index = 1; index <= workers; index += 1)
    expected.set(workerToken(runTag, index), index);
  const byId = new Map();
  // Provider retry state per team member (the lead included).
  const retries = new Map();
  // Times at which the lead's last answer turned into the final report; a
  // report before the last worker reply is a false one.
  const leadReports = [];
  let leadShowsReport = false;
  const text = (value) =>
    `${value.lastAnswer ?? ""}\n${value.overview?.result ?? ""}`;
  return {
    observe(value, atMs) {
      if (value.id === leadId || value.rootId === leadId) {
        const retry = retries.get(value.id) ?? {
          sinceMs: null,
          events: new Set(),
          waitMs: 0,
        };
        retries.set(value.id, retry);
        const status = value.nativeStatus;
        if (status?.phase === "retrying") {
          retry.events.add(status.at ?? atMs);
          retry.sinceMs ??= atMs;
        } else if (retry.sinceMs !== null) {
          retry.waitMs += atMs - retry.sinceMs;
          retry.sinceMs = null;
        }
      }
      if (value.id === leadId) {
        // The final answer only: the overview result carries interim text
        // and placeholders while the lead still waits for workers.
        const shows = (value.lastAnswer ?? "").includes(leadDoneToken(runTag));
        if (shows && !leadShowsReport) leadReports.push(atMs);
        leadShowsReport = shows;
        return;
      }
      if (value.rootId !== leadId) return;
      const row = byId.get(value.id) ?? {
        id: value.id,
        name: value.name ?? null,
        firstSeenAtMs: atMs,
        startedAtMs: null,
        completedAtMs: null,
        token: null,
        accountKey: null,
        status: null,
        error: null,
      };
      byId.set(value.id, row);
      row.status = value.status ?? null;
      row.accountKey = value.accountKey ?? row.accountKey;
      row.error = value.error ?? null;
      if (row.startedAtMs === null && (value.inFlight || value.turnId))
        row.startedAtMs = atMs;
      if (row.completedAtMs === null) {
        const token = [...expected.keys()].find((item) =>
          text(value).includes(item),
        );
        if (token) {
          row.token = token;
          row.completedAtMs = atMs;
        }
      }
    },
    workers: () => [...byId.values()],
    // Provider throttling seen as retry events: a lower bound, since several
    // events between two samples count once. Waits still open end at `endMs`.
    throttling(endMs) {
      let waitMs = 0;
      let events = 0;
      let agents = 0;
      for (const retry of retries.values()) {
        events += retry.events.size;
        agents += retry.events.size ? 1 : 0;
        waitMs +=
          retry.waitMs +
          (retry.sinceMs === null ? 0 : Math.max(0, endMs - retry.sinceMs));
      }
      return { events, agents, waitMs };
    },
    leadReports: () => [...leadReports],
    summary() {
      const rows = [...byId.values()];
      const tokens = new Set(
        rows.filter((row) => row.token).map((row) => row.token),
      );
      const lastReplyMs =
        tokens.size === workers
          ? Math.max(...rows.map((row) => row.completedAtMs ?? 0))
          : null;
      // The report that counts comes at or after the last worker reply.
      const leadReportedAtMs =
        lastReplyMs === null
          ? null
          : (leadReports.find((atMs) => atMs >= lastReplyMs) ?? null);
      return {
        expected: workers,
        visible: rows.length,
        started: rows.filter((row) => row.startedAtMs !== null).length,
        replied: tokens.size,
        missingTokens: [...expected.keys()].filter(
          (token) => !tokens.has(token),
        ),
        leadReportedAtMs,
        lastReplyMs,
        accountDistribution: Object.fromEntries(
          [
            ...rows.reduce((counts, row) => {
              const key = row.accountKey ?? "unknown";
              return counts.set(key, (counts.get(key) ?? 0) + 1);
            }, new Map()),
          ].sort(([a], [b]) => (a < b ? -1 : 1)),
        ),
      };
    },
  };
}

// First sample time at which `count(sample)` reached `target`, or null.
export function firstReachedMs(samples, count, target) {
  const hit = samples.find((sample) => count(sample) >= target);
  return hit ? hit.atMs : null;
}

// CPU milliseconds used between two samples by a set of processes. `previous`
// maps pid to cumulative CPU; a process new in `current` counts in full.
export function cpuDeltaMs(previous, current) {
  let total = 0;
  for (const { pid, cpuMs } of current)
    total += Math.max(0, cpuMs - (previous.get(pid) ?? 0));
  return total;
}

export function cpuPercent(deltaCpuMs, deltaWallMs) {
  return deltaWallMs > 0
    ? Math.round((deltaCpuMs * PERCENT * 10) / deltaWallMs) / 10
    : null;
}

// Terminal SQLite lock errors counted by the backend (diagnostics snapshot).
export function sqliteLockErrors(diagnostics) {
  const count = diagnostics?.sqliteContention?.lockError?.lockErrors;
  return Number.isSafeInteger(count) && count >= 0 ? count : null;
}

export function sqliteWriteWaits(diagnostics) {
  const lock = diagnostics?.sqliteContention?.lockError;
  return {
    count: lock?.writeWaits ?? null,
    totalMs: lock?.writeWaitMs ?? null,
  };
}

// Pass/fail decision. `metrics` holds only measured values; null means "not
// observed", which fails closed.
export function evaluateCriteria(metrics, workers) {
  const checks = [
    ["allAgentsVisibleInUi", metrics.uiAllVisibleMs !== null],
    ["allWorkerTurnsStarted", metrics.started === workers],
    ["allAgentsReplied", metrics.replied === workers],
    ["leadReported", metrics.leadReportedAtMs !== null],
    ["noSqliteLockErrors", metrics.sqliteLockErrorDelta === 0],
    [
      `frameGapUnder${FRAME_GAP_LIMIT_MS}ms`,
      metrics.maxFrameGapMs !== null &&
        metrics.maxFrameGapMs <= FRAME_GAP_LIMIT_MS,
    ],
    [
      `echoUnder${ECHO_LATENCY_LIMIT_MS}ms`,
      metrics.maxEchoMs !== null && metrics.maxEchoMs <= ECHO_LATENCY_LIMIT_MS,
    ],
    [
      `apiP95Under${API_P95_LIMIT_MS}ms`,
      metrics.apiP95Ms !== null && metrics.apiP95Ms <= API_P95_LIMIT_MS,
    ],
  ];
  return {
    passed: checks.every(([, ok]) => ok),
    checks: Object.fromEntries(checks),
    failures: checks.filter(([, ok]) => !ok).map(([name]) => name),
  };
}

export const USAGE = `Usage: node verify-ui-team-scale.mjs [options]
  --workers N          subagents to spawn, 1..${WORKERS_MAX} (default 3)
  --lead PROFILE       ${LEAD_PROFILE_NAMES.join(" | ")} (default codex-luna)
  --worker PROFILE     ${WORKER_PROFILE_NAMES.join(" | ")} (default codex-luna)
  --worker-workspace   shared | worktree (default shared)
  --project PATH       registered project (default: main checkout of this repository)
  --origin URL         backend (default http://127.0.0.1:4620)
  --evidence-dir DIR   absolute directory outside the checkout (default under ~/.cache)
  --journal-unit UNIT  systemd user unit of the backend whose journal is audited (default codex-studio)
  --keep               skip the cleanup so the chat can be inspected
Live run against a real backend: announce it. Headless Chromium only.`;

export function parseArgs(argv, defaults) {
  const options = {
    workers: 3,
    lead: "codex-luna",
    worker: "codex-luna",
    workerWorkspace: "shared",
    origin: "http://127.0.0.1:4620",
    journalUnit: "codex-studio",
    keep: false,
    help: false,
    ...defaults,
  };
  const value = (index, name) => {
    const next = argv[index + 1];
    assert.ok(
      next !== undefined && !next.startsWith("--"),
      `${name} needs a value.`,
    );
    return next;
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") options.help = true;
    else if (arg === "--keep") options.keep = true;
    else if (arg === "--workers") options.workers = Number(value(index++, arg));
    else if (arg === "--lead") options.lead = value(index++, arg);
    else if (arg === "--worker") options.worker = value(index++, arg);
    else if (arg === "--worker-workspace")
      options.workerWorkspace = value(index++, arg);
    else if (arg === "--project") options.project = value(index++, arg);
    else if (arg === "--origin") options.origin = value(index++, arg);
    else if (arg === "--journal-unit")
      options.journalUnit = value(index++, arg);
    else if (arg === "--evidence-dir")
      options.evidenceDir = value(index++, arg);
    else assert.fail(`Unknown option ${arg}.\n${USAGE}`);
  }
  assert.ok(
    Number.isSafeInteger(options.workers) &&
      options.workers >= 1 &&
      options.workers <= WORKERS_MAX,
    `--workers must be an integer from 1 to ${WORKERS_MAX}.`,
  );
  assert.ok(
    LEAD_PROFILE_NAMES.includes(options.lead),
    `--lead must be one of ${LEAD_PROFILE_NAMES.join(", ")}.`,
  );
  assert.ok(
    WORKER_PROFILE_NAMES.includes(options.worker),
    `--worker must be one of ${WORKER_PROFILE_NAMES.join(", ")}.`,
  );
  return options;
}

export function summaryText(journal) {
  const r = journal.result;
  const m = journal.metrics;
  const line = (label, value) => `${label.padEnd(34)}${value}`;
  return [
    `UI team scale run ${journal.runTag}: ${r.passed ? "PASSED" : "FAILED"}`,
    line(
      "workers / lead / worker profile",
      `${journal.options.workers} / ${journal.options.lead} / ${journal.options.worker}`,
    ),
    line("failures", r.failures.join(", ") || "none"),
    line(
      "first/all agents visible in UI (s)",
      `${sec(m.uiFirstVisibleMs)} / ${sec(m.uiAllVisibleMs)}`,
    ),
    line(
      "first/all agents in backend (s)",
      `${sec(m.backendFirstVisibleMs)} / ${sec(m.backendAllVisibleMs)}`,
    ),
    line(
      "workers started / replied",
      `${m.started} / ${m.replied} of ${journal.options.workers}`,
    ),
    line("lead reported (s)", sec(m.leadReportedAtMs)),
    ...(m.spawnBatches ?? []).map((b) =>
      line(
        `spawn call ${b.call} (agents ${b.from}-${b.to})`,
        `created ${b.created}, first/last seen ${sec(b.firstSeenAtMs)} / ${sec(b.lastSeenAtMs)} s, first start ${sec(b.firstStartedAtMs)} s, last reply ${sec(b.lastCompletedAtMs)} s, replied ${b.replied}`,
      ),
    ),
    line(
      "workers per account",
      Object.entries(m.accountDistribution ?? {})
        .map(([key, count]) => `${key}=${count}`)
        .join(" ") || "n/a",
    ),
    line(
      "provider retries (not a failure)",
      `${m.throttleEvents} events on ${m.throttledAgents} agents, waited ${sec(m.throttleWaitMs)} s in total`,
    ),
    line(
      "max frame gap / max echo (ms)",
      `${m.maxFrameGapMs} / ${m.maxEchoMs}`,
    ),
    line(
      "long tasks / longest (ms)",
      `${m.longTaskCount} / ${m.longestLongTaskMs}`,
    ),
    line(
      "API p95 (ms) / requests",
      `${m.apiP95Ms} / ${m.apiRequestCount} (diagnostics p95 ${m.diagnosticsP95Ms})`,
    ),
    line(
      "sqlite lock errors / write waits",
      `${m.sqliteLockErrorDelta} / ${m.sqliteWriteWaitDelta}`,
    ),
    line(
      "backend CPU% peak / RSS MiB peak",
      `${m.backendCpuPercentPeak} / ${m.backendRssMiBPeak}`,
    ),
    line(
      "provider CPU% peak / RSS MiB peak",
      `${m.providerCpuPercentPeak} / ${m.providerRssMiBPeak}`,
    ),
    ...(journal.audit?.queueGauges ?? []).map((gauge) =>
      line(
        `global ${gauge.path.split("/").at(-1)} before/end/delta`,
        `${gauge.before} / ${gauge.after} / ${gauge.delta}`,
      ),
    ),
    line("cleanup", journal.cleanup?.state ?? "not run"),
    ...(journal.audit ? auditSummaryLines(journal.audit) : ["audit: not run"]),
    "",
  ].join("\n");
}

function sec(ms) {
  return ms === null || ms === undefined ? "n/a" : (ms / 1000).toFixed(1);
}
