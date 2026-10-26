import type { RetainedInputRows } from "./verify-rust-chat-scale.ts";
import type { TranscriptWireItem } from "../src/apiTypes.ts";
import type { components, AgentEntityDto } from "../src/generated/api.ts";
import type { SpawnTimeline } from "./uiTeamScaleHelpers.ts";

// Audits also consume historical sparse transcript rows; every field comes from
// the generated transcript contract, including provider extension fields.
export type AuditInput = Pick<
  NonNullable<TranscriptWireItem["inputs"]>[number],
  "id" | "kind" | "at" | "text"
> &
  Partial<Pick<components["schemas"]["TranscriptInput"], "turnId">>;
export type AuditItem = Partial<
  Pick<
    TranscriptWireItem,
    | "id"
    | "role"
    | "at"
    | "turnId"
    | "turnStatus"
    | "toolStatus"
    | "text"
    | "title"
    | "phase"
  >
> & { inputs?: AuditInput[] | null | undefined };
export type AuditEntry = { kind: string; [field: string]: unknown };
export type Audit = {
  failures: AuditEntry[];
  anomalies: AuditEntry[];
  queueGauges?: ReturnType<typeof auditDiagnostics>["queueGauges"];
};
export type Counters = Record<string, number | null>;
type PendingInput = Pick<RetainedInputRows[number], "id" | "agent" | "status">;
type JsonObject = Record<string, components["schemas"]["JsonValue"]>;
import { RETAINED_INPUT_STATUSES } from "./scaleVerifierHelpers.ts";
import { readRetainedInputRows } from "./verify-rust-chat-scale.ts";

// Strict post-run audit of verify-ui-team-scale.ts: pure functions over the
// lead and worker transcripts, the lead's tool requests, the backend journal,
// diagnostics snapshots and the observed worker timeline. Each check returns
// failures (the run fails) and anomalies (recorded, the run may still pass).

// Transcripts fetched at once after a run, requests.
export const AUDIT_FETCH_CONCURRENCY = 16;
// Items per transcript page; the runtime's largest page (transcript_runtime
// MAX_PAGE_SIZE), items.
export const TRANSCRIPT_PAGE_LIMIT = 500;
// A lead input that waited longer than this for its turn is an anomaly,
// milliseconds.
export const LEAD_INPUT_WAIT_ANOMALY_MS = 60_000;
// A worker that started this much later than a worker queued after it on the
// same account is a start-order inversion, milliseconds.
export const START_ORDER_TOLERANCE_MS = 5_000;
// Journal lines kept per message class in the evidence.
export const JOURNAL_EXAMPLES_PER_CLASS = 3;
// Message classes listed in the summary text.
export const SUMMARY_CLASSES_MAX = 8;

const BAD_TURN_STATUSES = new Set([
  "failed",
  "interrupted",
  "cancelled",
  "aborted",
]);
const TOOL_ITEM_RESULT_STATUSES = new Set(["completed", "failed", "declined"]);
// Tool request stages that still wait for a result.
const UNRESOLVED_REQUEST_STAGES = new Set(["queued", "running", "unknown"]);
const ABANDONED_REQUEST_STAGES = new Set(["interrupted", "cancelled"]);
// Answers a model gives while it only waits for more input.
const PLACEHOLDER_ANSWERS = new Set(["", "...", "…"]);

// Journal lines that are per-operation tracing, not events.
const JOURNAL_TRACE =
  /runtime_stage name=|studio_provider_(stdin_write|reply) /;
const JOURNAL_ERROR = /\bERROR\b|\bpanic(ked)?\b|\bFATAL\b/;
const JOURNAL_SUSPICIOUS =
  /\bWARN(ING)?\b|failed|error|retry|retrying|timed out|timeout|lost|stuck|refused/i;
// Case-insensitive journalctl --grep pattern that keeps every line either
// class above can match, so the backend journal is not read in full.
export const JOURNAL_GREP = `${JOURNAL_ERROR.source}|${JOURNAL_SUSPICIOUS.source}`;

// Diagnostics counters that must not grow during a run (JSON pointer paths).
export const DIAGNOSTICS_ZERO_DELTA_PATHS = Object.freeze([
  "/queues/claudeLogLinesSkipped",
  "/sqliteContention/transaction/lockErrors",
  "/sqliteContention/lockError/lockErrors",
  "/sqliteContention/walTelemetry/canvas/busy",
  "/sqliteContention/walTelemetry/analytics/busy",
  "/sqliteContention/slowTransactions/activeTracking/untrackedStarts",
  "/httpRequestMetrics/untrackedRoutes",
]);
// Shared-server queue gauges are evidence only; owned snapshot rows decide pass.
export const DIAGNOSTICS_QUEUE_GAUGE_PATHS = Object.freeze([
  "/queues/durableInputPending",
  "/queues/recoveryPending",
]);

export function pointer(value: unknown, path: string): number | null {
  let current = value;
  for (const key of path.split("/").slice(1)) {
    if (current === null || typeof current !== "object") return null;
    current = (current as Record<string, unknown>)[key];
  }
  return typeof current === "number" && Number.isFinite(current)
    ? current
    : null;
}

export function diagnosticsCounters(diagnostics: unknown) {
  return Object.fromEntries(
    [...DIAGNOSTICS_ZERO_DELTA_PATHS, ...DIAGNOSTICS_QUEUE_GAUGE_PATHS].map(
      (path) => [path, pointer(diagnostics, path)],
    ),
  );
}

// The fenced snapshot publishes the same durable input status as diagnostics.
// Keep only identities and status; never retain another chat's input content.
export function pendingTeamInputs(events: PendingInput[], agentIds: string[]) {
  const owned = new Set<string | null | undefined>(agentIds);
  return events
    .filter(
      (event) =>
        owned.has(event.agent) &&
        RETAINED_INPUT_STATUSES.includes(event.status ?? ""),
    )
    .map(({ id, agent, status }) => ({ id, agent, status }));
}

// Shared gauges cannot attribute growth. Only the complete snapshot of owned
// pending inputs decides this audit, regardless of activity in other chats.
export function auditDiagnostics(
  before: Counters | undefined,
  after: Counters | undefined,
  pendingInputs: PendingInput[] = [],
) {
  const failures: AuditEntry[] = [];
  const anomalies: AuditEntry[] = [];
  for (const path of DIAGNOSTICS_ZERO_DELTA_PATHS) {
    const from = before?.[path] ?? null;
    const to = after?.[path] ?? null;
    if (from === null || to === null)
      anomalies.push({ kind: "diagnosticsCounterMissing", path });
    else if (to > from)
      failures.push({ kind: "diagnosticsCounterGrew", path, from, to });
  }
  const queueGauges = DIAGNOSTICS_QUEUE_GAUGE_PATHS.map((path) => {
    const from = before?.[path] ?? null;
    const to = after?.[path] ?? null;
    return {
      path,
      before: from,
      after: to,
      delta: from === null || to === null ? null : to - from,
    };
  });
  if (pendingInputs.length)
    failures.push({
      kind: "teamInputsPending",
      count: pendingInputs.length,
      inputs: pendingInputs,
    });
  return { failures, anomalies, queueGauges };
}

// The final-audit caller uses this complete source, never bounded event SSE.
export function auditTeamDiagnostics({
  before,
  after,
  stateDir,
  workspaceId,
  agentIds,
  deadlineAt,
}: {
  before: Counters | undefined;
  after: Counters | undefined;
  stateDir: string;
  workspaceId: string;
  agentIds: string[];
  deadlineAt: number;
}) {
  const pending = pendingTeamInputs(
    readRetainedInputRows(stateDir, workspaceId, deadlineAt),
    agentIds,
  );
  return {
    ...auditDiagnostics(before, after, pending),
    teamInputAudit: {
      source: "readRetainedInputRows: fenced read-only runtime_events snapshot",
      pending,
    },
  };
}

function parseJson(text: string | null | undefined): JsonObject | null {
  try {
    const value: unknown = JSON.parse(text ?? "");
    return value !== null && typeof value === "object" && !Array.isArray(value)
      ? (value as JsonObject)
      : null;
  } catch {
    return null;
  }
}

// Tool calls of a transcript: one per output item that carries a tool status.
export function toolCalls(items: AuditItem[]) {
  return items
    .filter(
      (item) => item.role === "output" && typeof item.toolStatus === "string",
    )
    .map((item) => {
      const body = parseJson(item.text ?? "") ?? {};
      return {
        callId: body.id ?? item.id,
        tool: body.tool ?? body.type ?? item.title ?? null,
        status: item.toolStatus,
        message: (Array.isArray(body.contentItems) ? body.contentItems : [])
          .map((part) =>
            part && typeof part === "object" && !Array.isArray(part)
              ? (part.text ?? "")
              : "",
          )
          .join(" | ")
          .slice(0, 300),
        turnId: item.turnId ?? null,
      };
    });
}

function turnsOf(items: AuditItem[]) {
  return new Set(
    items
      .map((item) => item.turnId)
      .filter((turn): turn is string => typeof turn === "string" && !!turn),
  );
}

function badTurns(items: AuditItem[]) {
  const found = new Map<
    NonNullable<AuditItem["turnId"]>,
    AuditItem["turnStatus"]
  >();
  for (const item of items)
    if (BAD_TURN_STATUSES.has(item.turnStatus ?? "") && item.turnId)
      found.set(item.turnId, item.turnStatus);
  return [...found].map(([turnId, status]) => ({ turnId, status }));
}

function inputsOf(items: AuditItem[]) {
  return items
    .filter((item) => item.role === "user")
    .flatMap((item) =>
      (item.inputs ?? []).map((input) => ({ ...input, itemAt: item.at })),
    );
}

function repeated<T>(values: T[]) {
  const counts = new Map<T, number>();
  for (const value of values) counts.set(value, (counts.get(value) ?? 0) + 1);
  return [...counts].filter(([, count]) => count > 1).map(([value]) => value);
}

// One worker: one task turn plus any unique lead-message turns. Delivery
// duplication, unexplained turns, failed turns and unanswered tools still fail.
export function auditWorker(id: string, items: AuditItem[], leadId?: string) {
  const failures: AuditEntry[] = [];
  const anomalies: AuditEntry[] = [];
  const turns = turnsOf(items);
  const triggeringInputs = new Map<string, AuditInput[]>();
  for (const [index, item] of items.entries()) {
    if (item.role !== "user") continue;
    // Older transcript rows omit turnId on the user item; the following
    // output identifies that input batch's turn, before the next user item.
    const following = items.slice(index + 1);
    const boundary = following.findIndex((next) => next.role === "user");
    const nextTurn = (
      boundary < 0 ? following : following.slice(0, boundary)
    ).find((next) => next.turnId)?.turnId;
    for (const input of item.inputs || []) {
      const turn = input.turnId || item.turnId || nextTurn;
      if (typeof turn === "string" && turn)
        triggeringInputs.set(turn, [
          ...(triggeringInputs.get(turn) || []),
          input,
        ]);
    }
  }
  const duplicateIds = new Set(
    repeated(inputsOf(items).map((input) => input.id)),
  );
  const leadTurns = new Set<string | null | undefined>();
  for (const turnId of [...turns].slice(1)) {
    const inputs = triggeringInputs.get(turnId) || [];
    if (
      inputs.length &&
      inputs.every(
        (input) =>
          input.id &&
          !duplicateIds.has(input.id) &&
          input.kind === "agent_message" &&
          leadId &&
          parseJson(input.text)?.sender === leadId,
      )
    ) {
      leadTurns.add(turnId);
      for (const input of inputs)
        anomalies.push({
          kind: "leadMessageTriggeredWorkerTurn",
          agent: id,
          turnId,
          input: input.id,
          text: parseJson(input.text)?.text ?? input.text,
        });
    }
  }
  if (turns.size - leadTurns.size > 1)
    failures.push({
      kind: "duplicateWorkerTurns",
      agent: id,
      turns: [...turns],
    });
  for (const turn of badTurns(items))
    failures.push({ kind: "failedWorkerTurn", agent: id, ...turn });
  const inputs = inputsOf(items);
  for (const input of repeated(inputs.map((entry) => entry.id)))
    failures.push({ kind: "duplicateDelivery", agent: id, input });
  if (!inputs.some((entry) => entry.kind === "user"))
    failures.push({ kind: "workerTaskNotDelivered", agent: id });
  for (const call of toolCalls(items)) {
    if (!TOOL_ITEM_RESULT_STATUSES.has(call.status ?? ""))
      failures.push({ kind: "toolCallWithoutResult", agent: id, ...call });
    else if (call.status !== "completed")
      anomalies.push({ kind: "workerToolCallFailed", agent: id, ...call });
  }
  return { failures, anomalies, turns: turns.size };
}

// Entity state at the end of the run: an error or a turn that did not complete.
export function auditWorkerEntity(
  entity: Pick<AgentEntityDto, "id" | "error" | "lastCompletedTurnStatus">,
) {
  const failures: AuditEntry[] = [];
  if (entity.error)
    failures.push({
      kind: "workerError",
      agent: entity.id,
      error: String(entity.error).slice(0, 300),
    });
  if (
    entity.lastCompletedTurnStatus &&
    entity.lastCompletedTurnStatus !== "completed"
  )
    failures.push({
      kind: "workerTurnNotCompleted",
      agent: entity.id,
      status: entity.lastCompletedTurnStatus,
    });
  return failures;
}

// The lead: no failed turn, every worker's child_result delivered exactly
// once and completed, every tool call answered; interim placeholder answers,
// failed tool calls, repeated final reports and long input waits are recorded.
export function auditLead(
  items: AuditItem[],
  workerIds: string[],
  doneToken: string,
) {
  const failures: AuditEntry[] = [];
  const anomalies: AuditEntry[] = [];
  for (const turn of badTurns(items))
    failures.push({ kind: "failedLeadTurn", ...turn });
  const inputs = inputsOf(items);
  for (const input of repeated(inputs.map((entry) => entry.id)))
    failures.push({ kind: "duplicateDelivery", agent: "lead", input });
  const delivered = new Map<components["schemas"]["JsonValue"], number>();
  for (const input of inputs.filter((entry) => entry.kind === "child_result")) {
    const result = parseJson(input.text ?? "");
    const agent = result?.agent_id ?? null;
    delivered.set(agent, (delivered.get(agent) ?? 0) + 1);
    if (result?.status !== "completed")
      failures.push({
        kind: "childResultNotCompleted",
        agent,
        status: result?.status ?? null,
      });
  }
  for (const [agent, count] of delivered)
    if (count > 1)
      failures.push({ kind: "duplicateChildResult", agent, count });
  const missing = workerIds.filter((id) => !delivered.has(id));
  if (missing.length)
    failures.push({
      kind: "missingChildResults",
      count: missing.length,
      agents: missing.slice(0, 20),
    });
  const unknown = [...delivered.keys()].filter(
    (id) => !(typeof id === "string" && workerIds.includes(id)),
  );
  if (unknown.length)
    failures.push({ kind: "childResultFromUnknownAgent", agents: unknown });
  for (const call of toolCalls(items)) {
    if (!TOOL_ITEM_RESULT_STATUSES.has(call.status ?? ""))
      failures.push({ kind: "toolCallWithoutResult", agent: "lead", ...call });
    else if (call.status !== "completed")
      anomalies.push({ kind: "leadToolCallFailed", ...call });
  }
  const answers = items.filter(
    (item) => item.role === "assistant" && item.phase === "final_answer",
  );
  const placeholders = answers.filter((item) =>
    PLACEHOLDER_ANSWERS.has((item.text ?? "").trim()),
  );
  if (placeholders.length)
    anomalies.push({
      kind: "leadPlaceholderAnswers",
      count: placeholders.length,
      texts: [...new Set(placeholders.map((item) => item.text))],
    });
  const reports = answers.filter((item) =>
    (item.text ?? "").includes(doneToken),
  );
  if (reports.length > 1)
    anomalies.push({ kind: "leadRepeatedFinalReport", count: reports.length });
  let maxWaitMs = 0;
  for (const input of inputs)
    if (
      typeof input.at === "number" &&
      typeof input.itemAt === "number" &&
      Number.isFinite(input.at) &&
      Number.isFinite(input.itemAt)
    )
      maxWaitMs = Math.max(maxWaitMs, (input.itemAt - input.at) * 1000);
  maxWaitMs = Math.round(maxWaitMs);
  if (maxWaitMs > LEAD_INPUT_WAIT_ANOMALY_MS)
    anomalies.push({ kind: "leadInputWaited", maxWaitMs });
  return {
    failures,
    anomalies,
    turns: turnsOf(items).size,
    childResults: delivered.size,
    maxInputWaitMs: maxWaitMs,
  };
}

// Durable tool requests of the lead against what the model was told: a
// request still without a result fails, and so does a request applied by the
// runtime while the model received a failure.
export function auditToolRequests(
  requests: Partial<components["schemas"]["ToolRequestRecordResponse"]>[],
  calls: ReturnType<typeof toolCalls>,
) {
  const failures: AuditEntry[] = [];
  const anomalies: AuditEntry[] = [];
  const byCall = new Map(calls.map((call) => [call.callId, call]));
  for (const request of requests) {
    const base = {
      request: request.id,
      tool: request.tool ?? null,
      stage: request.stage,
      outcome: request.outcome ?? null,
    };
    if (UNRESOLVED_REQUEST_STAGES.has(request.stage ?? ""))
      failures.push({ kind: "toolRequestWithoutResult", ...base });
    else if (ABANDONED_REQUEST_STAGES.has(request.stage ?? ""))
      anomalies.push({ kind: "toolRequestAbandoned", ...base });
    const call = byCall.get(request.callId ?? undefined);
    if (
      call &&
      request.stage === "completed" &&
      request.outcome === "applied" &&
      call.status !== "completed"
    )
      failures.push({
        kind: "toolOutcomeContradicted",
        ...base,
        told: call.status,
        message: call.message,
      });
  }
  return { failures, anomalies };
}

// journalctl --grep uses status 1 for an empty result as well as failures.
export function auditJournalResult(
  result: {
    status: number | null;
    stdout: string;
    stderr: string;
    error?: Error;
  },
  unit: string,
) {
  if (
    result.status === 0 ||
    (result.status === 1 &&
      result.stdout === "" &&
      result.stderr === "" &&
      !result.error)
  )
    return auditJournal((result.stdout || "").split("\n"));
  return {
    failures: [],
    anomalies: [
      {
        kind: "journalUnavailable",
        unit,
        error: String(result.error?.message ?? result.stderr ?? "").slice(
          0,
          300,
        ),
      },
    ],
  };
}

// Journal lines of the run window ("<iso time> <host> <unit>[pid]: text").
// ERROR and panic lines fail the run; other suspicious lines are grouped by
// message class as anomalies.
export function auditJournal(lines: string[]) {
  const classes = new Map<
    string,
    { severity: string; message: string; count: number; examples: string[] }
  >();
  for (const line of lines) {
    if (!line || JOURNAL_TRACE.test(line)) continue;
    const message = line.replace(/^\S+\s+\S+\s+[^:]+:\s*/, "");
    const severity = JOURNAL_ERROR.test(message)
      ? "error"
      : JOURNAL_SUSPICIOUS.test(message)
        ? "suspicious"
        : null;
    if (!severity) continue;
    const key = message
      .replace(/[0-9a-f]{8}-[0-9a-f-]{27}/gi, "<id>")
      .replace(/\d+(\.\d+)?/g, "<n>")
      .slice(0, 200);
    const entry = classes.get(key) ?? {
      severity,
      message: key,
      count: 0,
      examples: [],
    };
    entry.count += 1;
    if (entry.examples.length < JOURNAL_EXAMPLES_PER_CLASS)
      entry.examples.push(line.slice(0, 400));
    classes.set(key, entry);
  }
  const all = [...classes.values()].sort((a, b) => b.count - a.count);
  return {
    failures: all
      .filter((entry) => entry.severity === "error")
      .map((entry) => ({ kind: "backendErrorLog", ...entry })),
    anomalies: all
      .filter((entry) => entry.severity === "suspicious")
      .map((entry) => ({ kind: "backendLog", ...entry })),
  };
}

// Per account, workers that started later than a worker first seen after them
// (beyond the tolerance). Rows: {accountKey, firstSeenAtMs, startedAtMs}.
export function startOrderInversions(
  rows: Pick<SpawnTimeline, "accountKey" | "firstSeenAtMs" | "startedAtMs">[],
) {
  const byAccount = new Map<
    string,
    Array<
      Pick<SpawnTimeline, "accountKey"> & {
        firstSeenAtMs: number;
        startedAtMs: number;
      }
    >
  >();
  for (const row of rows) {
    if (row.startedAtMs === null || row.firstSeenAtMs === null) continue;
    const key = row.accountKey ?? "unknown";
    byAccount.set(key, [
      ...(byAccount.get(key) ?? []),
      {
        ...row,
        firstSeenAtMs: row.firstSeenAtMs,
        startedAtMs: row.startedAtMs,
      },
    ]);
  }
  const anomalies: AuditEntry[] = [];
  for (const [accountKey, list] of byAccount) {
    list.sort((a, b) => a.firstSeenAtMs - b.firstSeenAtMs);
    let count = 0;
    let worstMs = 0;
    for (const [index, row] of list.entries()) {
      const laterStart = list
        .slice(index + 1)
        .filter(
          (other) =>
            other.firstSeenAtMs - row.firstSeenAtMs > START_ORDER_TOLERANCE_MS,
        )
        .reduce((best, other) => Math.min(best, other.startedAtMs), Infinity);
      const lateMs = row.startedAtMs - laterStart;
      if (lateMs > START_ORDER_TOLERANCE_MS) {
        count += 1;
        worstMs = Math.max(worstMs, lateMs);
      }
    }
    if (count)
      anomalies.push({
        kind: "startOrderInversion",
        accountKey,
        workers: count,
        worstMs: Math.round(worstMs),
      });
  }
  return anomalies;
}

// A lead final report earlier than the last worker reply is a false report.
export function auditLeadReport(
  leadReports: number[],
  lastReplyMs: number | null,
) {
  if (!leadReports.length || lastReplyMs === null) return [];
  const early = leadReports.filter((atMs) => atMs < lastReplyMs);
  return early.length
    ? [
        {
          kind: "leadReportedBeforeAllReplied",
          reportsMs: early,
          lastReplyMs,
        },
      ]
    : [];
}

// Runs `task` over `values` with at most `limit` in flight.
export async function mapLimited<T, R>(
  values: T[],
  limit: number,
  task: (value: T, index: number) => Promise<R>,
): Promise<R[]> {
  const results: R[] = [];
  const entries = values.entries();
  const lane = async () => {
    for (;;) {
      const entry = entries.next();
      if (entry.done) return;
      const [index, value] = entry.value;
      results[index] = await task(value, index);
    }
  };
  // An early rejected lane must not let teardown delete the team while other
  // transcript reads are still in flight.
  const lanes = await Promise.allSettled(
    Array.from({ length: Math.min(limit, values.length) }, lane),
  );
  const failure = lanes.find((result) => result.status === "rejected");
  if (failure) throw failure.reason;
  return results;
}

export function mergeAudits(parts: Partial<Audit>[]) {
  return {
    failures: parts.flatMap((part) => part.failures ?? []),
    anomalies: parts.flatMap((part) => part.anomalies ?? []),
  };
}

// Summary lines: failure kinds with counts, then anomaly kinds.
export function auditSummaryLines(audit: Audit) {
  const tally = (list: AuditEntry[]) => {
    const counts = new Map<AuditEntry["kind"], number>();
    for (const entry of list)
      counts.set(entry.kind, (counts.get(entry.kind) ?? 0) + 1);
    return [...counts].map(([kind, count]) => `${kind}×${count}`).join(", ");
  };
  // The first entry of each anomaly kind, then the rest, up to the limit.
  const kinds = new Set<string>();
  const firsts = audit.anomalies.filter(
    (entry) => !kinds.has(entry.kind) && kinds.add(entry.kind),
  );
  const detail = [...new Set([...firsts, ...audit.anomalies])]
    .slice(0, SUMMARY_CLASSES_MAX)
    .map((entry) => `  - ${describe(entry)}`);
  return [
    `audit failures: ${tally(audit.failures) || "none"}`,
    `anomalies: ${tally(audit.anomalies) || "none"}`,
    ...detail,
  ];
}

function describe(entry: AuditEntry) {
  const { kind, examples, ...rest } = entry;
  void examples;
  return `${kind} ${JSON.stringify(rest).slice(0, 240)}`;
}
