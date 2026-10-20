import { describe, expect, it } from "vitest";
import {
  LEAD_INPUT_WAIT_ANOMALY_MS,
  START_ORDER_TOLERANCE_MS,
  auditDiagnostics,
  auditJournal,
  auditLead,
  auditLeadReport,
  auditSummaryLines,
  auditToolRequests,
  auditWorker,
  auditWorkerEntity,
  diagnosticsCounters,
  mapLimited,
  mergeAudits,
  startOrderInversions,
  toolCalls,
} from "../scripts/uiTeamScaleAudit.mjs";

// Shapes follow the persisted transcript items of a live 512-worker run.
const userItem = (at, inputs) => ({ role: "user", at, inputs });
const childInput = (agent, at, status = "completed") => ({
  id: `child:${agent}:turn`,
  kind: "child_result",
  at,
  text: JSON.stringify({ agent_id: agent, result: "TOKEN", status }),
});
const toolItem = (id, tool, status, message = "", turnId = "t1") => ({
  role: "output",
  toolStatus: status,
  turnId,
  turnStatus: "completed",
  text: JSON.stringify({
    id,
    tool,
    type: "dynamicToolCall",
    status,
    contentItems: message ? [{ text: message, type: "inputText" }] : [],
  }),
});
const answer = (text, turnId = "t1") => ({
  role: "assistant",
  phase: "final_answer",
  text,
  turnId,
  turnStatus: "completed",
});
const kinds = (audit) => audit.failures.map((entry) => entry.kind);
const anomalyKinds = (audit) => audit.anomalies.map((entry) => entry.kind);

describe("worker audit", () => {
  const healthy = [
    {
      role: "user",
      at: 10,
      inputs: [{ id: "w1:initial", kind: "user", at: 1, text: "task" }],
    },
    {
      role: "output",
      toolStatus: "completed",
      turnId: "t1",
      turnStatus: "completed",
      text: JSON.stringify({ id: "exec-1", type: "commandExecution" }),
    },
    answer("TOKEN"),
  ];

  it("passes one delivered task, one turn and answered tool calls", () => {
    const audit = auditWorker("w1", healthy);
    expect(audit).toEqual({ failures: [], anomalies: [], turns: 1 });
  });

  it("fails duplicate turns, failed turns, duplicate deliveries and unanswered calls", () => {
    const items = [
      ...healthy,
      healthy[0],
      { ...answer("again", "t2"), turnStatus: "failed" },
      { ...toolItem("exec-2", "x", "inProgress"), turnId: "t2" },
    ];
    expect(kinds(auditWorker("w1", items))).toEqual([
      "duplicateWorkerTurns",
      "failedWorkerTurn",
      "duplicateDelivery",
      "toolCallWithoutResult",
    ]);
  });

  it("fails a worker whose task never arrived", () => {
    expect(kinds(auditWorker("w1", [answer("x")]))).toEqual([
      "workerTaskNotDelivered",
    ]);
  });

  it("fails an entity with an error or a turn that did not complete", () => {
    expect(
      auditWorkerEntity({ id: "w1", lastCompletedTurnStatus: "completed" }),
    ).toEqual([]);
    expect(
      auditWorkerEntity({
        id: "w1",
        error: "exceeded retry limit, last status: 429",
        lastCompletedTurnStatus: "failed",
      }).map((entry) => entry.kind),
    ).toEqual(["workerError", "workerTurnNotCompleted"]);
  });
});

describe("lead audit", () => {
  const workers = ["a", "b"];

  it("passes each child result delivered once", () => {
    const audit = auditLead(
      [
        userItem(100, [{ id: "u", kind: "user", at: 100, text: "go" }]),
        userItem(130, [childInput("a", 120), childInput("b", 125)]),
        answer("TAG_ALL_REPLIED", "t2"),
      ],
      workers,
      "TAG_ALL_REPLIED",
    );
    expect(audit).toMatchObject({
      failures: [],
      anomalies: [],
      childResults: 2,
      maxInputWaitMs: 10_000,
    });
  });

  it("fails duplicate, missing, unknown and failed child results", () => {
    const audit = auditLead(
      [
        userItem(10, [childInput("a", 10), childInput("x", 10, "failed")]),
        userItem(20, [{ ...childInput("a", 11), id: "child:a:other" }]),
      ],
      workers,
      "TAG_ALL_REPLIED",
    );
    expect(kinds(audit)).toEqual([
      "childResultNotCompleted",
      "duplicateChildResult",
      "missingChildResults",
      "childResultFromUnknownAgent",
    ]);
  });

  it("fails a failed lead turn and an input delivered twice", () => {
    const input = childInput("a", 1);
    const audit = auditLead(
      [
        userItem(2, [input, childInput("b", 1)]),
        userItem(3, [input]),
        { ...answer("", "t9"), turnStatus: "interrupted" },
      ],
      workers,
      "TAG_ALL_REPLIED",
    );
    expect(kinds(audit)).toEqual([
      "failedLeadTurn",
      "duplicateDelivery",
      "duplicateChildResult",
    ]);
  });

  it("records placeholder answers, failed tool calls, repeated reports and long waits", () => {
    const waitSeconds = LEAD_INPUT_WAIT_ANOMALY_MS / 1000 + 1;
    const audit = auditLead(
      [
        toolItem(
          "exec-1",
          "orchestration_spawn",
          "failed",
          "Too many unresolved provider replies",
        ),
        answer("TAG_ALL_REPLIED"),
        userItem(10 + waitSeconds, [childInput("a", 10), childInput("b", 10)]),
        answer("", "t2"),
        answer("...", "t3"),
        answer("TAG_ALL_REPLIED", "t4"),
      ],
      workers,
      "TAG_ALL_REPLIED",
    );
    expect(audit.failures).toEqual([]);
    expect(anomalyKinds(audit)).toEqual([
      "leadToolCallFailed",
      "leadPlaceholderAnswers",
      "leadRepeatedFinalReport",
      "leadInputWaited",
    ]);
    expect(audit.anomalies[0]).toMatchObject({
      tool: "orchestration_spawn",
      message: "Too many unresolved provider replies",
    });
    expect(audit.anomalies[1]).toMatchObject({ count: 2, texts: ["", "..."] });
  });

  it("fails a final report made before the last worker reply", () => {
    expect(auditLeadReport([17_500, 1_115_000], 1_112_000)).toEqual([
      {
        kind: "leadReportedBeforeAllReplied",
        reportsMs: [17_500],
        lastReplyMs: 1_112_000,
      },
    ]);
    expect(auditLeadReport([1_115_000], 1_112_000)).toEqual([]);
    expect(auditLeadReport([5], null)).toEqual([]);
  });
});

describe("tool request audit", () => {
  const calls = toolCalls([
    toolItem("exec-1", "orchestration_spawn", "failed", "Too many unresolved"),
    toolItem("exec-2", "orchestration_status", "completed"),
  ]);

  it("reads tool calls from transcript items", () => {
    expect(calls[0]).toMatchObject({
      callId: "exec-1",
      tool: "orchestration_spawn",
      status: "failed",
      message: "Too many unresolved",
    });
  });

  it("fails an applied request that the model was told failed", () => {
    const audit = auditToolRequests(
      [
        {
          id: "r1",
          callId: "exec-1",
          tool: "orchestration_spawn",
          stage: "completed",
          outcome: "applied",
        },
        { id: "r2", callId: "exec-2", stage: "completed", outcome: "applied" },
      ],
      calls,
    );
    expect(kinds(audit)).toEqual(["toolOutcomeContradicted"]);
    expect(audit.failures[0]).toMatchObject({ request: "r1", told: "failed" });
  });

  it("fails requests without a result and records abandoned ones", () => {
    const audit = auditToolRequests(
      [
        { id: "q", stage: "queued" },
        { id: "r", stage: "running" },
        { id: "u", stage: "unknown" },
        { id: "i", stage: "interrupted" },
        { id: "f", stage: "failed", outcome: "not_applied" },
      ],
      [],
    );
    expect(kinds(audit)).toEqual([
      "toolRequestWithoutResult",
      "toolRequestWithoutResult",
      "toolRequestWithoutResult",
    ]);
    expect(anomalyKinds(audit)).toEqual(["toolRequestAbandoned"]);
  });
});

describe("journal audit", () => {
  const prefix =
    "2026-10-20T11:43:27.123456+02:00 host studio-runtime-server[1]:";
  const lines = [
    `${prefix} runtime_stage name=journal.compact elapsed_ms=3 failed=0`,
    `${prefix} studio_provider_reply {"error":null}`,
    `${prefix} provider history import failed: analytics history file changed while reading; checkpoint retained`,
    `${prefix} provider history import failed: analytics history file changed while reading; checkpoint retained`,
    `${prefix} ERROR internal HTTP error route=GET /api/session-cost request_id=unavailable class=unclassified`,
    `${prefix} thread 'tokio-runtime-worker' panicked at src/main.rs:10:5`,
    `${prefix} runtime server startup checkpoint: listener-bound`,
    "",
  ];

  it("fails ERROR and panic lines and groups suspicious lines", () => {
    const audit = auditJournal(lines);
    expect(audit.failures.map((entry) => entry.message)).toEqual([
      "ERROR internal HTTP error route=GET /api/session-cost request_id=unavailable class=unclassified",
      "thread 'tokio-runtime-worker' panicked at src/main.rs:<n>:<n>",
    ]);
    expect(audit.anomalies).toHaveLength(1);
    expect(audit.anomalies[0]).toMatchObject({
      kind: "backendLog",
      count: 2,
      message:
        "provider history import failed: analytics history file changed while reading; checkpoint retained",
    });
  });
});

describe("diagnostics audit", () => {
  it("fails a growing counter or a pending queue and records missing paths", () => {
    const before = diagnosticsCounters({
      queues: { claudeLogLinesSkipped: 0 },
      sqliteContention: { lockError: { lockErrors: 1 } },
    });
    const after = diagnosticsCounters({
      queues: {
        claudeLogLinesSkipped: 2,
        durableInputPending: 1,
        recoveryPending: 0,
      },
      sqliteContention: { lockError: { lockErrors: 1 } },
    });
    const audit = auditDiagnostics(before, after);
    expect(audit.failures).toEqual([
      {
        kind: "diagnosticsCounterGrew",
        path: "/queues/claudeLogLinesSkipped",
        from: 0,
        to: 2,
      },
      {
        kind: "diagnosticsGaugeNotZero",
        path: "/queues/durableInputPending",
        value: 1,
      },
    ]);
    expect(
      anomalyKinds(audit).every((kind) => kind === "diagnosticsCounterMissing"),
    ).toBe(true);
    expect(audit.anomalies.map((entry) => entry.path)).not.toContain(
      "/sqliteContention/lockError/lockErrors",
    );
  });
});

describe("start order", () => {
  const late = START_ORDER_TOLERANCE_MS * 2;

  it("records a worker started after one queued later on its account", () => {
    expect(
      startOrderInversions([
        { accountKey: "a", firstSeenAtMs: 0, startedAtMs: late * 3 },
        { accountKey: "a", firstSeenAtMs: late, startedAtMs: late },
        { accountKey: "b", firstSeenAtMs: 0, startedAtMs: late * 3 },
        { accountKey: "b", firstSeenAtMs: 1, startedAtMs: 0 },
        { accountKey: "c", firstSeenAtMs: 0, startedAtMs: null },
      ]),
    ).toEqual([
      {
        kind: "startOrderInversion",
        accountKey: "a",
        workers: 1,
        worstMs: late * 2,
      },
    ]);
  });
});

describe("helpers", () => {
  it("bounds concurrency and keeps the order", async () => {
    let active = 0;
    let peak = 0;
    const results = await mapLimited([3, 1, 2, 0], 2, async (value) => {
      active += 1;
      peak = Math.max(peak, active);
      await new Promise((resolve) => setTimeout(resolve, value));
      active -= 1;
      return value * 10;
    });
    expect(results).toEqual([30, 10, 20, 0]);
    expect(peak).toBe(2);
  });

  it("summarises failures and anomalies by kind", () => {
    const audit = mergeAudits([
      { failures: [{ kind: "missingChildResults" }] },
      {
        anomalies: [
          { kind: "backendLog", message: "m", count: 9, examples: ["x"] },
        ],
      },
    ]);
    expect(auditSummaryLines(audit)).toEqual([
      "audit failures: missingChildResults×1",
      "anomalies: backendLog×1",
      '  - backendLog {"message":"m","count":9}',
    ]);
  });
});
