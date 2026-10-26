import type {
  Audit,
  AuditItem,
  AuditInput,
} from "../scripts/uiTeamScaleAudit.ts";
import { describe, expect, it } from "vitest";
import {
  LEAD_INPUT_WAIT_ANOMALY_MS,
  START_ORDER_TOLERANCE_MS,
  auditDiagnostics,
  pendingTeamInputs,
  auditJournal,
  auditJournalResult,
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
} from "../scripts/uiTeamScaleAudit.ts";

// Shapes follow the persisted transcript items of a live 512-worker run.
const userItem = (at: number, inputs: AuditInput[]): AuditItem => ({
  role: "user",
  at,
  inputs,
});
const childInput = (
  agent: string,
  at: number,
  status = "completed",
): AuditInput => ({
  id: `child:${agent}:turn`,
  kind: "child_result",
  at,
  text: JSON.stringify({ agent_id: agent, result: "TOKEN", status }),
});
const toolItem = (
  id: string,
  tool: string,
  status: string,
  message = "",
  turnId = "t1",
): AuditItem => ({
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
const answer = (text: string, turnId = "t1"): AuditItem => ({
  role: "assistant",
  phase: "final_answer",
  text,
  turnId,
  turnStatus: "completed",
});
const kinds = (audit: Audit) => audit.failures.map((entry) => entry.kind);
const anomalyKinds = (audit: Audit) =>
  audit.anomalies.map((entry) => entry.kind);

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
  ] satisfies AuditItem[];

  it("passes one delivered task, one turn and answered tool calls", () => {
    const audit = auditWorker("w1", healthy);
    expect(audit).toEqual({ failures: [], anomalies: [], turns: 1 });
  });

  it("fails duplicate turns, failed turns, duplicate deliveries and unanswered calls", () => {
    const items = [
      ...healthy,
      healthy.at(0) ?? userItem(10, []),
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
      sqliteContention: { lockError: { lockErrors: 1 } },
      queues: {
        claudeLogLinesSkipped: 0,
        durableInputPending: 0,
        recoveryPending: 0,
      },
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
    const pending = Array.from({ length: 4 }, () =>
      Promise.withResolvers<void>(),
    );
    const started: number[] = [];
    const results = mapLimited([3, 1, 2, 0], 2, async (value, index) => {
      started.push(value);
      const deferred = pending[index];
      if (!deferred) throw new Error("Missing deferred fixture");
      await deferred.promise;
      return value * 10;
    });
    expect(started).toEqual([3, 1]);
    pending.at(1)?.resolve();
    await pending.at(1)?.promise;
    await Promise.resolve();
    expect(started).toEqual([3, 1, 2]);
    pending.at(2)?.resolve();
    pending.at(0)?.resolve();
    pending.at(3)?.resolve();
    expect(await results).toEqual([30, 10, 20, 0]);
  });

  it("passes undefined entries to the task without leaving result holes", async () => {
    expect(
      await mapLimited([undefined, 1], 2, async (value, index) => ({
        value,
        index,
      })),
    ).toEqual([
      { value: undefined, index: 0 },
      { value: 1, index: 1 },
    ]);
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

it("records global queue growth as evidence and rejects only retained team inputs", () => {
  const before = diagnosticsCounters({
    queues: { durableInputPending: 13, recoveryPending: 2 },
  });
  const after = diagnosticsCounters({
    queues: { durableInputPending: 12, recoveryPending: 1 },
  });
  expect(auditDiagnostics(before, after).failures).toEqual([]);
  const pending = pendingTeamInputs(
    [
      { id: "foreign", agent: "other", status: "pending" },
      { id: "done", agent: "lead", status: "completed" },
      { id: "own", agent: "worker", status: "dispatching" },
    ],
    ["lead", "worker"],
  );
  expect(pending).toEqual([
    { id: "own", agent: "worker", status: "dispatching" },
  ]);
  // A team input fails even if another chat drains and masks its global growth.
  expect(auditDiagnostics(before, after, pending).failures).toEqual([
    { kind: "teamInputsPending", count: 1, inputs: pending },
  ]);
  const grown = diagnosticsCounters({
    queues: { durableInputPending: 14, recoveryPending: 2 },
  });
  expect(auditDiagnostics(before, grown).failures).toEqual([]);
  expect(auditDiagnostics(before, grown).queueGauges).toContainEqual({
    path: "/queues/durableInputPending",
    before: 13,
    after: 14,
    delta: 1,
  });
  expect(
    auditDiagnostics(before, grown, pending).failures.map(({ kind }) => kind),
  ).toContain("teamInputsPending");
});

it("treats journalctl's empty status 1 as no matches, while preserving other failures", () => {
  expect(
    auditJournalResult({ status: 1, stdout: "", stderr: "" }, "studio"),
  ).toEqual({ failures: [], anomalies: [] });
  expect(
    auditJournalResult(
      { status: 1, stdout: "", stderr: "No journal access" },
      "studio",
    ).anomalies,
  ).toEqual([
    { kind: "journalUnavailable", unit: "studio", error: "No journal access" },
  ]);
  expect(
    auditJournalResult(
      { status: 2, stdout: "", stderr: "" },
      "studio",
    ).anomalies.at(0)?.kind,
  ).toBe("journalUnavailable");
});

it("attributes only a new lead message triggering another worker turn to the lead", () => {
  const initial = {
    ...userItem(1, [{ id: "initial", kind: "user", text: "task" }]),
    turnId: "t1",
  };
  const message = {
    id: "nudge",
    kind: "agent_message",
    text: JSON.stringify({
      sender: "lead",
      text: "Please finish the assigned exact shell command",
    }),
  };
  const audit = auditWorker(
    "worker",
    [
      initial,
      answer("TOKEN"),
      { ...userItem(2, [message]), turnId: "t2" },
      answer("TOKEN", "t2"),
    ],
    "lead",
  );
  expect(audit.failures).toEqual([]);
  expect(audit.anomalies).toEqual([
    {
      kind: "leadMessageTriggeredWorkerTurn",
      agent: "worker",
      turnId: "t2",
      input: "nudge",
      text: "Please finish the assigned exact shell command",
    },
  ]);
  expect(
    kinds(
      auditWorker(
        "worker",
        [initial, answer("TOKEN"), answer("TOKEN", "t2")],
        "lead",
      ),
    ),
  ).toContain("duplicateWorkerTurns");
  expect(
    kinds(
      auditWorker(
        "worker",
        [
          initial,
          answer("TOKEN"),
          { ...userItem(2, [message]), turnId: "t2" },
          answer("TOKEN", "t2"),
          { ...userItem(3, [message]), turnId: "t3" },
          answer("TOKEN", "t3"),
        ],
        "lead",
      ),
    ),
  ).toEqual(
    expect.arrayContaining(["duplicateWorkerTurns", "duplicateDelivery"]),
  );
  expect(
    kinds(
      auditWorker(
        "worker",
        [
          initial,
          answer("TOKEN"),
          {
            ...userItem(2, [
              {
                ...message,
                text: JSON.stringify({ sender: "other", text: "go" }),
              },
            ]),
            turnId: "t2",
          },
          answer("TOKEN", "t2"),
        ],
        "lead",
      ),
    ),
  ).toContain("duplicateWorkerTurns");
});
