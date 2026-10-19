import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import {
  closeSync,
  constants as fsConstants,
  openSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  watch,
  writeSync,
} from "node:fs";
import { lstat, mkdir, open, readFile, rename, unlink } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";

// Approved paid-test profiles. Never use an account's default model: it can
// resolve to an expensive one. `servedModelPrefix` is the owner of the family
// that catalogue ids and served usage models must start with; `usageSource`
// filters the usage ledger (null: every non-Claude-response row);
// `processName` is the provider child process name sampled for RSS and CPU.
// Every profile spreads over the ready accounts of its provider (the lead uses
// the first); `accountKey` is only the fallback when discovery is off.
export const SCALE_PROFILES = Object.freeze({
  "claude-haiku": Object.freeze({
    provider: "claude",
    accountKey: "claude-local",
    model: "haiku",
    effort: "low",
    fastMode: false,
    servedModelPrefix: "claude-haiku-",
    usageSource: "claudeResponse",
    processName: "claude",
  }),
  "claude-sonnet": Object.freeze({
    provider: "claude",
    accountKey: "claude-local",
    model: "sonnet",
    effort: "low",
    fastMode: false,
    servedModelPrefix: "claude-sonnet-",
    usageSource: "claudeResponse",
    processName: "claude",
  }),
  "codex-luna": Object.freeze({
    provider: "codex",
    accountKey: "default",
    model: "gpt-6-luna",
    effort: "low",
    fastMode: false,
    servedModelPrefix: "gpt-6-luna",
    usageSource: null,
    processName: "codex",
  }),
});
export const SCALE_PROVIDER = SCALE_PROFILES["claude-haiku"];

// Owner of the upstream-limit evidence pattern used by the scale verifier.
// Word boundaries keep identifiers such as hex operation ids (which can contain
// "429") from matching; only standalone status codes and limit phrases count.
export const UPSTREAM_LIMIT_PATTERN =
  /\brate[ _-]?limit|\b429\b|\bcapacity\b|\busage limit|\btoo many requests\b/i;

export function showsUpstreamLimit(text) {
  return UPSTREAM_LIMIT_PATTERN.test(String(text ?? ""));
}

export function scaleProfile(name) {
  const profile = SCALE_PROFILES[name];
  assert.ok(
    profile,
    `Profile must be one of ${Object.keys(SCALE_PROFILES).join(", ")}.`,
  );
  return profile;
}

// Ready accounts of the profile's provider. The accounts route reports Codex
// accounts without a provider field.
export function scaleAccountKeys(accounts, profile) {
  const keys = accounts
    .filter(
      (row) =>
        row.status === "ready" &&
        (row.provider ?? "codex") === profile.provider,
    )
    .map((row) => row.id)
    .sort();
  assert.ok(
    keys.length > 0,
    `No ready ${profile.provider} account is available.`,
  );
  return keys;
}

// With several accounts the lead's defaults must not pin one: the create path
// rejects a worker whose explicit account differs from a pinned default.
export function scaleWorkerDefaults(model, profile, accountKeys) {
  return {
    ...(accountKeys.length === 1 ? { account_key: accountKeys[0] } : {}),
    model,
    effort: profile.effort,
    fast_mode: profile.fastMode,
    delegation_depth: 1,
  };
}

// The approved load profile requires an isolated Git worktree for every worker.
export const SCALE_WORKSPACE_MODE = "worktree";

export function callbackWorktreeCwd(entity, agentId, project) {
  assert.ok(
    entity && typeof entity.cwd === "string" && isAbsolute(entity.cwd),
    `Worker ${agentId} has no authoritative absolute worktree cwd.`,
  );
  assert.equal(
    entity.workspaceMode,
    SCALE_WORKSPACE_MODE,
    `Worker ${agentId} is not using its requested worktree mode.`,
  );
  // SyncAgentEntity publishes worktree and worktreePreparation; the private
  // stored worktreeReady flag is not part of this public projection.
  assert.equal(
    entity.worktree,
    true,
    `Worker ${agentId} has no published worktree.`,
  );
  assert.ok(
    entity.worktreePreparation == null,
    `Worker ${agentId} worktree preparation is still pending.`,
  );
  const cwd = resolve(entity.cwd);
  assert.notEqual(
    cwd,
    project,
    `Worker ${agentId} did not receive an isolated worktree.`,
  );
  return cwd;
}

export function requireScaleModel(catalog, profile = SCALE_PROVIDER) {
  const candidate = catalog.data?.find((row) => row.model === profile.model);
  assert.ok(
    candidate,
    `${profile.model} is unavailable in the live account catalogue; no agents were created.`,
  );
  // Codex catalogues publish no resolved id: the model itself is the identity.
  const identity = candidate.resolvedModel ?? candidate.model;
  assert.ok(
    typeof identity === "string" &&
      identity.startsWith(profile.servedModelPrefix),
    `The paid-test model must resolve to ${profile.servedModelPrefix}*; no agents were created.`,
  );
  assert.ok(
    candidate.supportedReasoningEfforts?.some(
      (row) => row.reasoningEffort === profile.effort,
    ),
    `${profile.model} does not expose ${profile.effort} effort.`,
  );
  return candidate;
}

export const PROVIDER_STDIN_JOURNAL_PREFIX = "studio_provider_stdin_write ";
export const PROVIDER_REPLY_JOURNAL_PREFIX = "studio_provider_reply ";
// Requests whose stdin write and reply are journaled besides turn/start. Their
// lines never join an exact input event.
export const THREAD_REQUEST_METHODS = ["thread/start", "thread/resume"];

// Linux exposes UUID text in /proc; journald stores the same boot ID as 32 hex digits.
export function canonicalJournalBootId(value) {
  assert.ok(
    typeof value === "string" &&
      /^(?:[0-9a-f]{32}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i.test(
        value,
      ),
    "Could not establish a valid kernel boot ID for journal scoping.",
  );
  return value.replaceAll("-", "").toLowerCase();
}

export function parseProviderStdinJournalMessage(message) {
  if (!message.startsWith(PROVIDER_STDIN_JOURNAL_PREFIX)) return null;
  const entry = JSON.parse(message.slice(PROVIDER_STDIN_JOURNAL_PREFIX.length));
  assert.deepEqual(
    Object.keys(entry).sort(),
    [
      "generation",
      "method",
      "operationId",
      "providerHandle",
      "requestId",
      "stdinFlushedAtNs",
    ],
    "Provider stdin journal record has an unexpected shape.",
  );
  assert.ok(
    typeof entry.providerHandle === "string" && entry.providerHandle.length,
    "Provider stdin journal record has no provider handle.",
  );
  assert.ok(
    Number.isSafeInteger(entry.generation) && entry.generation > 0,
    "Provider stdin journal record has an invalid generation.",
  );
  assert.ok(
    typeof entry.operationId === "string" && entry.operationId.length,
    "Provider stdin journal record has no operation ID.",
  );
  assert.ok(
    (typeof entry.requestId === "string" && entry.requestId.length > 0) ||
      (Number.isSafeInteger(entry.requestId) && entry.requestId >= 0),
    "Provider stdin journal record has an invalid request ID.",
  );
  assert.ok(
    typeof entry.method === "string" && entry.method.length > 0,
    "Provider stdin journal record has no method.",
  );
  assert.ok(
    Number.isSafeInteger(entry.stdinFlushedAtNs) && entry.stdinFlushedAtNs >= 0,
    "Provider stdin journal record has an invalid flush timestamp.",
  );
  return entry;
}

export function parseProviderReplyJournalMessage(message) {
  if (!message.startsWith(PROVIDER_REPLY_JOURNAL_PREFIX)) return null;
  const entry = JSON.parse(message.slice(PROVIDER_REPLY_JOURNAL_PREFIX.length));
  assert.deepEqual(
    Object.keys(entry).sort(),
    [
      "generation",
      "method",
      "operationId",
      "outcome",
      "providerHandle",
      "replyAtNs",
      "requestId",
    ],
    "Provider reply journal record has an unexpected shape.",
  );
  assert.ok(
    typeof entry.providerHandle === "string" &&
      typeof entry.operationId === "string" &&
      typeof entry.method === "string" &&
      Number.isSafeInteger(entry.generation) &&
      (typeof entry.requestId === "string" ||
        Number.isSafeInteger(entry.requestId)) &&
      Number.isSafeInteger(entry.replyAtNs) &&
      entry.replyAtNs >= 0 &&
      (entry.outcome === "result" || entry.outcome === "error"),
    "Provider reply journal record has an invalid field.",
  );
  return entry;
}

export async function forEachBounded(items, concurrency, operation) {
  for (let offset = 0; offset < items.length; offset += concurrency) {
    const batch = items.slice(offset, offset + concurrency);
    const results = await Promise.allSettled(
      batch.map((item, index) => operation(item, offset + index)),
    );
    const failure = results.find((result) => result.status === "rejected");
    if (failure) throw failure.reason;
  }
}

export function createEntityCursorPuller(
  initialSequence,
  workspaceId,
  pullPage,
) {
  assert.ok(Number.isSafeInteger(initialSequence) && initialSequence >= 0);
  let cursor = initialSequence;
  return async function pullThrough(targetSequence, { force = false } = {}) {
    assert.ok(Number.isSafeInteger(targetSequence) && targetSequence >= cursor);
    const after = cursor;
    const latestById = new Map();
    while (cursor < targetSequence || force) {
      force = false;
      const page = await pullPage(cursor);
      assert.equal(page.workspaceId, workspaceId, "Sync workspace changed.");
      assert.notEqual(
        page.reset,
        true,
        "Sync entity sequence reset during observation.",
      );
      assert.ok(Array.isArray(page.documents), "Sync pull omitted documents.");
      const checkpoint = page.checkpoint?.seq;
      assert.ok(
        Number.isSafeInteger(checkpoint) && checkpoint >= cursor,
        "Sync pull returned an invalid checkpoint.",
      );
      for (const document of page.documents) {
        assert.ok(
          typeof document.id === "string" && Number.isSafeInteger(document.seq),
          "Sync pull returned an invalid entity row.",
        );
        latestById.set(document.id, document);
      }
      if (checkpoint === cursor) {
        assert.ok(
          cursor >= targetSequence,
          "Sync pull did not advance toward the SSE invalidation sequence.",
        );
        break;
      }
      cursor = checkpoint;
    }
    return {
      after,
      through: cursor,
      documents: [...latestById.values()].sort((a, b) => a.seq - b.seq),
    };
  };
}

export function stateResourceVersion(value) {
  if (
    !Array.isArray(value?.resources) ||
    !Array.isArray(value.resourceVersions)
  )
    return null;
  const index = value.resources.findIndex(
    (resource) => resource.kind === "state",
  );
  return index < 0 ? null : value.resourceVersions[index] || null;
}

export function createStateResourceAdapter(
  workspaceId,
  observedAgentIds,
  pullPage,
) {
  let sequence = null;
  let lastFrameSequence = null;
  let pullThrough;
  return async function observe(frame, pendingSequence = null) {
    const version = stateResourceVersion(frame);
    assert.ok(
      Number.isSafeInteger(version?.revision),
      "State resource event omitted its entity-sequence revision.",
    );
    assert.equal(frame.workspaceId, workspaceId, "Sync workspace changed.");
    if (sequence === null) {
      assert.equal(
        frame.reason,
        "initial",
        "State stream began without its initial baseline.",
      );
      sequence = version.revision;
      pullThrough = createEntityCursorPuller(sequence, workspaceId, pullPage);
    } else {
      assert.notEqual(
        frame.reason,
        "initial",
        "State stream repeated its initial baseline.",
      );
      assert.ok(
        version.revision >= lastFrameSequence,
        `Sync entity sequence moved backwards: ${version.revision} < ${lastFrameSequence}.`,
      );
    }
    lastFrameSequence = version.revision;
    // Rust also emits entitySequenceReset for a broad post-commit invalidation.
    // Pull through the durable cursor; an actual pull reset still fails closed
    // in createEntityCursorPuller instead of treating every hint as data loss.
    const target = Math.max(
      sequence,
      pendingSequence ?? version.revision,
      version.revision,
    );
    const batch = await pullThrough(target, { force: true });
    sequence = batch.through;
    const latestById = new Map();
    for (const document of batch.documents) {
      const match = /^entity:agent:(.+)$/.exec(document.id);
      if (match && observedAgentIds.has(match[1]))
        latestById.set(document.id, document);
    }
    return {
      sequence,
      stateDocuments: [...latestById.values()].sort(
        (left, right) => left.seq - right.seq,
      ),
    };
  };
}

export function resumeIntentState(intent, existedBeforeResume) {
  if (existedBeforeResume && intent.state === "prepared") {
    return {
      ...intent,
      state: "unknown",
      error:
        "The process ended after writing the cleanup intent; inspect state without repeating the command.",
    };
  }
  return intent;
}

// The stored profile is what the backend will bill, so it is checked after the
// create response rather than trusting the request that asked for it.
export function assertStoredScaleProfile(
  entity,
  { id, model, effort, fastMode },
) {
  assert.ok(entity, `Agent ${id} has no stored profile.`);
  assert.equal(
    entity.model,
    model,
    `Agent ${id} stored model ${entity.model}, not the approved ${model}.`,
  );
  assert.equal(
    entity.effort,
    effort,
    `Agent ${id} stored effort ${entity.effort}, not the approved ${effort}.`,
  );
  assert.equal(
    entity.fastMode,
    fastMode,
    `Agent ${id} stored fastMode ${entity.fastMode}, not the approved ${fastMode}.`,
  );
}

// Served-model proof is the bridge's per-response model in the usage ledger.
// It is family evidence only: a stored record cannot show whether the bridge
// fell back to the configured model when the SDK omitted the response model.
export function assertServedModelEvidence(
  records,
  agentId,
  minimumResponses,
  profile = SCALE_PROVIDER,
) {
  assert.ok(
    servedModelTurnCount(records, agentId, profile) >= minimumResponses,
    `Agent ${agentId} has ${records.length} provider usage record(s) for ${minimumResponses} exchange(s); the served model is unproven.`,
  );
}

function servedModelTurnCount(records, agentId, profile) {
  for (const record of records)
    assert.ok(
      typeof record.model === "string" &&
        record.model.startsWith(profile.servedModelPrefix),
      `Agent ${agentId} was served ${record.model ?? "no model"}; the paid-test profile requires ${profile.servedModelPrefix}*.`,
    );
  return new Set(
    records
      .map((record) => record.turnId)
      .filter((id) => typeof id === "string" && id.length > 0),
  ).size;
}

// Completion of a native turn can precede its analytics publication. Re-read
// only after an existing publication event; the caller owns the same deadline.
export async function waitForServedModelEvidence({
  profile = SCALE_PROVIDER,
  agentIds,
  minimumResponses,
  readRecords,
  waitForPublication,
}) {
  for (;;) {
    const records = await readRecords();
    let complete = true;
    for (const agentId of agentIds)
      if (
        servedModelTurnCount(records.get(agentId) || [], agentId, profile) <
        minimumResponses
      )
        complete = false;
    if (complete) return records;
    await waitForPublication();
  }
}

// The runtime writes a numeric RequestId into a ticket as its decimal string
// (safety_buffering/executor.rs request_id_string), so both forms share one key.
// Anything else has no identity and returns null, which never joins.
export function requestIdentityKey(value) {
  if (typeof value === "string") return value.length ? value : null;
  return Number.isSafeInteger(value) && value >= 0 ? String(value) : null;
}

// Live rows outside the journal-owned set are foreign to this run's cleanup.
export function foreignLiveTeamRows(rows, ownedIds) {
  return rows.filter((row) => row.deletedAt == null && !ownedIds.has(row.id));
}

// A cleanup is finished once the row is deleted and no cleanup intent remains.
export function finishedWorkspaceCleanupIds(rows, expectedIds) {
  const byId = new Map(rows.map((row) => [row.id, row]));
  return new Set(
    [...expectedIds].filter((id) => {
      const row = byId.get(id);
      return Boolean(row?.deletedAt) && !row.workspaceCleanupIntent;
    }),
  );
}

// Projection of diagnostics/store.rs retained statuses, checked by the parity
// test. The exact-identity query builds its placeholders from this projection.
export const RETAINED_INPUT_STATUSES = Object.freeze([
  "pending",
  "reserved",
  "dispatching",
  "uncertain",
]);

// Projection of lifecycle_effects.rs child-result kind, checked by the parity
// test. A paused parent retains these notices until cleanup.
export const PARENT_NOTICE_INPUT_KIND = "child_result";

// Projection of diagnostics and provider_ports.rs queue keys, checked by the
// parity test. Counts are items; durable inputs are reconciled by identity.
export const DURABLE_INPUT_QUEUE_KEY = "durableInputPending";
const RECOVERY_QUEUE_KEY = "recoveryPending";
const ACCOUNT_QUEUE_NAMES = Object.freeze([
  "callbacks",
  "clock_replies",
  "tool_requests",
]);
const ACCOUNT_QUEUE_KEY = new RegExp(
  `^account[1-9]\\d*\\.(${ACCOUNT_QUEUE_NAMES.join("|")})$`,
);

function isKnownQueueKey(key) {
  return (
    key === RECOVERY_QUEUE_KEY ||
    key === DURABLE_INPUT_QUEUE_KEY ||
    ACCOUNT_QUEUE_KEY.test(key)
  );
}

function isCount(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

// Stage-end drain of the non-durable dispatch, callback, clock, and recovery
// queues. A missing, null, negative, fractional, or unrecognized counter is
// unavailable, never drained.
export function classifyStageQueues(queues) {
  const snapshot = queues ?? {};
  const keys = Object.keys(snapshot);
  const aliases = [
    ...new Set(
      keys
        .filter((key) => ACCOUNT_QUEUE_KEY.test(key))
        .map((key) => key.slice(0, key.indexOf("."))),
    ),
  ];
  const expected = [
    RECOVERY_QUEUE_KEY,
    DURABLE_INPUT_QUEUE_KEY,
    ...aliases.flatMap((alias) =>
      ACCOUNT_QUEUE_NAMES.map((name) => `${alias}.${name}`),
    ),
  ];
  const missing = expected.filter((key) => !keys.includes(key));
  if (!aliases.length) missing.push("account[N].*");
  const unknown = keys
    .filter((key) => !isKnownQueueKey(key) || !isCount(snapshot[key]))
    .map((key) => [key, snapshot[key]]);
  const nonzero = keys
    .filter(
      (key) =>
        key !== DURABLE_INPUT_QUEUE_KEY &&
        isKnownQueueKey(key) &&
        isCount(snapshot[key]) &&
        snapshot[key] !== 0,
    )
    .map((key) => [key, snapshot[key]]);
  const state =
    missing.length || unknown.length
      ? "unavailable"
      : nonzero.length
        ? "not-drained"
        : "drained";
  return {
    state,
    nonzero,
    unknown,
    missing,
    durableInputPending: isCount(snapshot[DURABLE_INPUT_QUEUE_KEY])
      ? snapshot[DURABLE_INPUT_QUEUE_KEY]
      : null,
  };
}

const RETAINED_PHASES = Object.freeze(["beforeCleanup", "afterCleanup"]);

// Exact-identity audit of durable inputs from one read-only snapshot. Foreign
// rows must match the pre-write baseline by identity, agent, kind, and status:
// a consumed, changed, or newly retained foreign input contaminates the
// workload even when the aggregate count is unchanged. Owned rows may remain
// only before cleanup, and only as a parent's pending child-result notices;
// after the owned deletes none may remain.
export function classifyRetainedInputs({
  rows,
  baseline,
  ownedAgentIds,
  parentAgentIds,
  phase,
}) {
  assert.ok(
    RETAINED_PHASES.includes(phase),
    `Unknown retained-input phase: ${phase}.`,
  );
  for (const row of rows)
    assert.ok(
      typeof row.id === "string" &&
        row.id.length > 0 &&
        typeof row.agent === "string" &&
        typeof row.kind === "string" &&
        RETAINED_INPUT_STATUSES.includes(row.status),
      "Retained-input evidence has a malformed row.",
    );
  const foreign = rows.filter((row) => !ownedAgentIds.has(row.agent));
  const foreignById = new Map(foreign.map((row) => [row.id, row]));
  const baselineIds = new Set(baseline.map((entry) => entry.id));
  const changed = baseline
    .filter((entry) => {
      const current = foreignById.get(entry.id);
      return (
        !current ||
        current.agent !== entry.agent ||
        current.kind !== entry.kind ||
        current.status !== entry.status
      );
    })
    .map((entry) => entry.id);
  const introduced = foreign
    .filter((row) => !baselineIds.has(row.id))
    .map((row) => row.id);
  assert.ok(
    changed.length === 0 && introduced.length === 0,
    `Foreign retained inputs changed during the workload (consumed or changed: ${JSON.stringify(changed)}; introduced: ${JSON.stringify(introduced)}); the workload is contaminated.`,
  );
  const owned = rows.filter((row) => ownedAgentIds.has(row.agent));
  const allowed =
    phase === "beforeCleanup"
      ? owned.filter(
          (row) =>
            parentAgentIds.has(row.agent) &&
            row.kind === PARENT_NOTICE_INPUT_KIND &&
            row.status === "pending",
        )
      : [];
  const allowedIds = new Set(allowed.map((row) => row.id));
  const unexpected = owned
    .filter((row) => !allowedIds.has(row.id))
    .map((row) => `${row.id} (${row.kind}, ${row.status})`);
  assert.ok(
    unexpected.length === 0,
    `${phase}: owned input(s) remain retained: ${unexpected.join(", ")}.`,
  );
  return {
    phase,
    foreignRetained: foreign.length,
    foreignBaseline: baseline.length,
    ownedRetained: owned.length,
    allowedParentNoticeIds: allowed.map((row) => row.id).sort(),
  };
}

export function normalizedWorktreePath(path) {
  assert.ok(
    typeof path === "string" && isAbsolute(path),
    "Owned worktree has no absolute path.",
  );
  return resolve(path);
}

export function createBoundedEventQueue(limit) {
  const events = [];
  let failure;
  return {
    get length() {
      return events.length;
    },
    push(value) {
      if (events.length >= limit) {
        failure = new Error(
          `State event buffer exceeded its ${limit} item limit.`,
        );
        return false;
      }
      events.push(value);
      return true;
    },
    shift() {
      return events.shift();
    },
    get failure() {
      return failure;
    },
  };
}

export async function acquireRunLock(path, runId) {
  const owner = { runId, pid: process.pid, token: randomUUID() };
  for (;;) {
    try {
      const handle = await open(path, "wx", 0o600);
      await handle.writeFile(`${JSON.stringify(owner)}\n`);
      await handle.close();
      return async () => {
        const current = JSON.parse(await readFile(path, "utf8"));
        assert.deepEqual(current, owner, "Run lock ownership changed.");
        await unlink(path);
      };
    } catch (error) {
      if (error?.code !== "EEXIST") throw error;
    }
    let current;
    try {
      current = JSON.parse(await readFile(path, "utf8"));
    } catch (error) {
      throw new Error(`Existing run lock is unreadable; inspect ${path}.`, {
        cause: error,
      });
    }
    assert.equal(
      current.runId,
      runId,
      "Evidence is locked by a different run.",
    );
    let alive = true;
    try {
      process.kill(current.pid, 0);
    } catch (error) {
      if (error?.code === "ESRCH") alive = false;
      else throw error;
    }
    assert.ok(
      !alive,
      `Run ${runId} is already active in process ${current.pid}.`,
    );
    const stalePath = `${path}.stale-${current.pid}-${randomUUID()}`;
    try {
      await rename(path, stalePath);
    } catch (error) {
      if (error?.code === "ENOENT") continue;
      throw error;
    }
  }
}

export function releaseFifoBarriers(
  workers,
  deadlineAt,
  processSet = new Set(),
) {
  const paths = workers.map((worker) => worker.path);
  const tokens = workers.map((worker) => worker.token);
  assert.ok(
    workers.every(
      (worker) =>
        isAbsolute(worker.path) && /^[0-9a-f-]{36}$/i.test(worker.token),
    ),
    "Workers must contain absolute FIFO paths and UUID release tokens.",
  );
  const remaining = Math.min(240_000, deadlineAt - performance.now());
  assert.ok(remaining > 0, "Stage deadline elapsed before FIFO release.");
  const script = String.raw`
const fs = require("node:fs");
const pathModule = require("node:path");
for (let index = 1; index < process.argv.length; index += 2) {
  const path = process.argv[index];
  const token = process.argv[index + 1];
  if (!pathModule.isAbsolute(path) || !/^[0-9a-f-]{36}$/i.test(token)) throw new Error("Unexpected worker identity.");
  if (!fs.lstatSync(path).isFIFO()) throw new Error("Expected an owned FIFO.");
  const fd = fs.openSync(path, "w");
  try { fs.writeSync(fd, token + "\n"); } finally { fs.closeSync(fd); }
}
`;
  const seenPaths = new Set(paths);
  const seenTokens = new Set(tokens);
  assert.equal(
    seenPaths.size,
    paths.length,
    "Release FIFO paths must be unique.",
  );
  assert.equal(
    seenTokens.size,
    tokens.length,
    "Release tokens must be unique.",
  );
  const child = spawn(
    process.execPath,
    ["-e", script, ...workers.flatMap((worker) => [worker.path, worker.token])],
    {
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  processSet.add(child);
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on(
    "data",
    (chunk) => (stderr = `${stderr}${chunk}`.slice(-1_000)),
  );
  return new Promise((resolve, reject) => {
    let timedOut = false;
    let killTimer;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
      killTimer = setTimeout(() => child.kill("SIGKILL"), 1_000);
    }, remaining);
    child.once("close", (code, signal) => {
      clearTimeout(timer);
      clearTimeout(killTimer);
      processSet.delete(child);
      if (!timedOut && code === 0) resolve();
      else
        reject(
          new Error(
            `FIFO release helper exited ${code ?? signal}: ${stderr.trim()}`,
          ),
        );
    });
  });
}

// Owner of the bounded shell-callback lifetime, in whole seconds. The GNU
// timeout kills the FIFO shell after the max, then waits the kill grace.
export const TOOL_BARRIER_MAX_SECONDS = 239;
export const TOOL_BARRIER_KILL_GRACE_SECONDS = 1;
// Enclosing timeout plus kill grace, with one second of scheduling margin.
export const TOOL_BARRIER_PROCESS_BOUND_MS =
  (TOOL_BARRIER_MAX_SECONDS + TOOL_BARRIER_KILL_GRACE_SECONDS + 1) * 1_000;

const READINESS_MARKER_PATTERN = /^worker-(0|[1-9]\d*)\.ready$/;

export function readinessMarkerName(index) {
  return `worker-${index}.ready`;
}

export function assertOwnedEntry(info, path, kind, mode) {
  assert.ok(!info.isSymbolicLink(), `${path} is a symbolic link.`);
  assert.equal(
    info.uid,
    process.getuid(),
    `${path} is not owned by the verifier.`,
  );
  assert.equal(info.mode & 0o777, mode, `${path} has unexpected permissions.`);
  assert.ok(kind(info), `${path} has an unexpected file type.`);
}

// Creates a fresh private marker directory. It never reuses or deletes an
// existing path, so previous evidence stays intact.
export async function createReadinessMarkerDirectory(path) {
  await mkdir(path, { mode: 0o700 });
  assertOwnedEntry(
    await lstat(path),
    path,
    (info) => info.isDirectory(),
    0o700,
  );
  return path;
}

// Arms the directory watch before any worker is created, then inspects the
// directory once so a marker published before arming is not missed. A marker
// is accepted only when its final name holds the exact per-worker token.
export async function watchReadinessMarkers(directory, tokens, signal = null) {
  signal?.throwIfAborted();
  const ready = [];
  const seen = new Set();
  const checks = new Map();
  const waiters = new Set();
  let failure = null;
  let closed = false;
  const wake = () => {
    for (const wakeWaiter of waiters) wakeWaiter();
  };
  const fail = (error) => {
    failure ??= error;
    wake();
  };
  const inspect = async (index) => {
    if (seen.has(index)) return;
    const path = join(directory, readinessMarkerName(index));
    const info = await lstat(path).catch((error) => {
      if (error?.code === "ENOENT") return null;
      throw error;
    });
    if (!info) return;
    assertOwnedEntry(info, path, (entry) => entry.isFile(), 0o600);
    assert.equal(info.nlink, 1, `${path} has extra hard links.`);
    assert.equal(
      await readFile(path, "utf8"),
      `${tokens[index]}\n`,
      `Worker ${index} readiness marker does not hold its exact token.`,
    );
    seen.add(index);
    ready.push(index);
    wake();
  };
  // An event arriving during an in-flight read must schedule another inspection.
  // Chain checks per worker so an initial ENOENT cannot swallow its rename event.
  const check = (index) => {
    const pending = checks.get(index) ?? Promise.resolve();
    checks.set(index, pending.then(() => inspect(index)).catch(fail));
  };
  const watcher = watch(directory, (_eventType, filename) => {
    if (closed) return;
    if (filename == null) {
      for (let index = 0; index < tokens.length; index += 1) check(index);
      return;
    }
    const name = String(filename);
    // A temporary name exists only until the atomic rename publishes the final name.
    if (name.endsWith(".tmp")) return;
    const match = READINESS_MARKER_PATTERN.exec(name);
    const index = match ? Number(match[1]) : Number.NaN;
    if (!Number.isSafeInteger(index) || index >= tokens.length)
      fail(new Error(`Unknown readiness marker name: ${name}`));
    else check(index);
  });
  watcher.on("error", fail);
  const onAbort = () =>
    fail(signal.reason ?? new Error("Readiness wait was aborted."));
  signal?.addEventListener("abort", onAbort, { once: true });
  for (let index = 0; index < tokens.length; index += 1) check(index);
  return {
    path: (index) => join(directory, readinessMarkerName(index)),
    async next(deadlineAt) {
      while (true) {
        if (failure) throw failure;
        if (closed) throw new Error("Readiness marker watch is closed.");
        if (ready.length) return ready.shift();
        const remaining = deadlineAt - performance.now();
        if (remaining <= 0)
          throw new Error("Timed out waiting for worker readiness markers.");
        await new Promise((resolveWait) => {
          const wakeWaiter = () => {
            clearTimeout(timer);
            waiters.delete(wakeWaiter);
            resolveWait();
          };
          const timer = setTimeout(wakeWaiter, remaining);
          waiters.add(wakeWaiter);
        });
      }
    },
    close() {
      if (closed) return;
      closed = true;
      watcher.close();
      signal?.removeEventListener("abort", onAbort);
      wake();
    },
  };
}

// Time a released callback shell needs to remove its FIFO and exit, in
// milliseconds. Owner of the bound used instead of the full timeout lifetime.
export const CALLBACK_EXIT_AFTER_RELEASE_MS = 5_000;

// Writes each owned release token without blocking. A FIFO with no reader
// (ENXIO) or no path (ENOENT) means its shell is already gone.
export function releaseBarriersNow(entries) {
  return entries.map(({ path, token }) => {
    assert.match(token, /^[0-9a-f-]{36}$/i, "Expected a UUID release token.");
    let fd;
    try {
      fd = openSync(path, fsConstants.O_WRONLY | fsConstants.O_NONBLOCK);
      writeSync(fd, `${token}\n`);
      return { path, outcome: "released" };
    } catch (error) {
      if (error?.code === "ENXIO" || error?.code === "ENOENT")
        return { path, outcome: "gone" };
      throw error;
    } finally {
      if (fd !== undefined) closeSync(fd);
    }
  });
}

// A verified readiness marker is emitted after the enclosing timeout starts.
// Creation can wait in a queue, so its intent timestamp cannot bound process exit.
// A cleanup that released the callback bounds exit from the release instead.
export function callbackProcessRetirementDelayMs({
  readinessObservedAt,
  releasedAt = null,
  nowMs = Date.now(),
}) {
  if (releasedAt) {
    const releasedMs = Date.parse(releasedAt);
    assert.ok(Number.isFinite(releasedMs), "Invalid callback release time.");
    return Math.max(0, releasedMs + CALLBACK_EXIT_AFTER_RELEASE_MS - nowMs);
  }
  const startMs = Date.parse(readinessObservedAt);
  assert.ok(
    Number.isFinite(startMs),
    "Owned callback has no verified readiness bound; worktree retirement is unproven.",
  );
  return Math.max(0, startMs + TOOL_BARRIER_PROCESS_BOUND_MS - nowMs);
}

// Provider child processes below a root, read from /proc (Linux). `comm` or the
// first two argv entries name the process, so `node .../claude` and
// `codex app-server` both match. CPU is cumulative user+system time.
const CLOCK_TICKS_PER_SECOND = 100;
const PAGE_SIZE_BYTES = 4096;

export function readProcessTable(procRoot = "/proc") {
  const table = new Map();
  for (const entry of readdirSync(procRoot)) {
    if (!/^[1-9]\d*$/.test(entry)) continue;
    try {
      const stat = readFileSync(`${procRoot}/${entry}/stat`, "utf8");
      // comm may contain spaces and parentheses; fields continue after the last ")".
      const rest = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
      const comm = stat.slice(stat.indexOf("(") + 1, stat.lastIndexOf(")"));
      const statm = readFileSync(`${procRoot}/${entry}/statm`, "utf8").split(
        " ",
      );
      const argv = readFileSync(`${procRoot}/${entry}/cmdline`, "utf8")
        .split("\0")
        .slice(0, 2);
      table.set(Number(entry), {
        pid: Number(entry),
        ppid: Number(rest[1]),
        comm,
        argv,
        cpuMs: Math.round(
          ((Number(rest[11]) + Number(rest[12])) * 1_000) /
            CLOCK_TICKS_PER_SECOND,
        ),
        rssBytes: Number(statm[1]) * PAGE_SIZE_BYTES,
      });
    } catch (error) {
      if (error?.code !== "ENOENT" && error?.code !== "ESRCH") throw error;
    }
  }
  return table;
}

export function sampleProviderProcesses(table, rootPid, processName) {
  const children = new Map();
  for (const item of table.values())
    children.set(item.ppid, [...(children.get(item.ppid) || []), item]);
  const tree = [];
  const queue = [...(children.get(rootPid) || [])];
  while (queue.length) {
    const item = queue.pop();
    tree.push(item);
    queue.push(...(children.get(item.pid) || []));
  }
  const names = (item) => [
    item.comm,
    ...item.argv.map((arg) => arg.split("/").at(-1)),
  ];
  const matched = tree.filter((item) => names(item).includes(processName));
  const rss = matched.map((item) => item.rssBytes).sort((a, b) => a - b);
  const at = (p) =>
    rss[Math.min(rss.length - 1, Math.ceil(p * rss.length) - 1)];
  const sum = (items, key) =>
    items.reduce((total, item) => total + item[key], 0);
  return {
    processName,
    backendRssBytes: table.get(rootPid)?.rssBytes ?? null,
    treeProcesses: tree.length,
    treeRssBytes: sum(tree, "rssBytes"),
    processes: matched.length,
    totalRssBytes: sum(matched, "rssBytes"),
    totalCpuMs: sum(matched, "cpuMs"),
    rssBytes: matched.length
      ? { p50: at(0.5), p95: at(0.95), max: rss.at(-1) }
      : null,
    perProcess: matched.map(({ pid, rssBytes, cpuMs }) => ({
      pid,
      rssBytes,
      cpuMs,
    })),
  };
}

// Where the workers of a stage run. `shared` puts every worker in the project
// checkout (workers do not edit files); `worktree` gives each its own Git
// worktree. The 512-worker target uses `shared`; separate 30/128 checks use
// `worktree`.
export const WORKER_WORKSPACES = Object.freeze(["shared", "worktree"]);
export const DEFAULT_WORKER_WORKSPACE = "shared";

export function parseWorkerWorkspace(value = DEFAULT_WORKER_WORKSPACE) {
  assert.ok(
    WORKER_WORKSPACES.includes(value),
    `--worker-workspace must be one of ${WORKER_WORKSPACES.join(", ")}.`,
  );
  return value;
}

// Per-worker release FIFO of a shared-workspace stage. It lives under the
// owned evidence directory, never inside the repository checkout.
export function sharedBarrierPaths(evidenceDir, workers, index) {
  assert.ok(isAbsolute(evidenceDir), "Evidence directory must be absolute.");
  assert.ok(
    Number.isSafeInteger(workers) && workers > 0,
    "Expected a positive worker count.",
  );
  assert.ok(
    Number.isSafeInteger(index) && index >= 0 && index < workers,
    "Expected a worker index inside the stage.",
  );
  const directory = join(evidenceDir, `fifos-${workers}`, `worker-${index}`);
  return { directory, path: join(directory, "release.fifo") };
}

// Processes holding each FIFO open, read from /proc/<pid>/fd. A FIFO shell
// keeps its read-write descriptor for as long as it blocks, so a holder proves
// the tool process is alive and still waiting for its release.
export function findFifoHolders(paths, procRoot = "/proc") {
  const wanted = new Map();
  for (const path of paths) {
    let real = path;
    try {
      real = realpathSync(path);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
    wanted.set(real, path);
  }
  const holders = new Map(paths.map((path) => [path, []]));
  for (const entry of readdirSync(procRoot)) {
    if (!/^[1-9]\d*$/.test(entry)) continue;
    let fds;
    try {
      fds = readdirSync(`${procRoot}/${entry}/fd`);
    } catch (error) {
      if (["ENOENT", "ESRCH", "EACCES", "EPERM"].includes(error?.code))
        continue;
      throw error;
    }
    for (const fd of fds) {
      let target;
      try {
        target = readlinkSync(`${procRoot}/${entry}/fd/${fd}`);
      } catch (error) {
        if (["ENOENT", "ESRCH", "EACCES", "EPERM"].includes(error?.code))
          continue;
        throw error;
      }
      const original = wanted.get(target);
      if (original !== undefined) {
        const pids = holders.get(original);
        if (!pids.includes(Number(entry))) pids.push(Number(entry));
      }
    }
  }
  return holders;
}

// Concurrency criterion for a stage. "Concurrently working" is: every worker
// turn reached the model (a physical turn/start stdin write per worker; the
// orchestrator's own turn is the extra one), and every worker's tool process is
// alive at the same moment, still holding its release FIFO. Open turns are not
// required: Codex yields a long-running tool after 30 s and the turn may end
// while the command keeps running.
export function assertConcurrentWorkers({
  workers,
  extraTurnStarts = 1,
  journalEntries,
  fifoPaths,
  holders,
}) {
  assert.equal(
    fifoPaths.length,
    workers,
    `Expected ${workers} worker FIFOs; found ${fifoPaths.length}.`,
  );
  const turnStarts = new Set(
    journalEntries
      .filter((entry) => entry.method === "turn/start")
      .map((entry) => entry.operationId),
  ).size;
  assert.ok(
    turnStarts >= workers + extraTurnStarts,
    `Only ${turnStarts} physical turn/start writes reached the model; ${workers + extraTurnStarts} are required.`,
  );
  const missing = fifoPaths.filter((path) => !holders.get(path)?.length);
  assert.equal(
    missing.length,
    0,
    `${missing.length} of ${workers} tool processes are not alive and holding their FIFO.`,
  );
  return {
    workers,
    physicalTurnStarts: turnStarts,
    toolProcessesHoldingFifo: workers,
    provedAt: new Date().toISOString(),
  };
}
