import { describe, expect, it } from "vitest";
import {
  ECHO_LATENCY_LIMIT_MS,
  FRAME_GAP_LIMIT_MS,
  SPAWN_BATCH_MAX,
  STAGE_DEADLINE_BASE_MS,
  STAGE_DEADLINE_PER_WORKER_MS,
  THROTTLE_WAVE_MS,
  ACCOUNT_CONCURRENT_REQUESTS,
  buildLeadPrompt,
  cpuDeltaMs,
  cpuPercent,
  createTeamTracker,
  evaluateCriteria,
  firstReachedMs,
  parseArgs,
  spawnBatchTimings,
  spawnCalls,
  percentile,
  sqliteLockErrors,
  stageDeadlineMs,
  workerToken,
} from "../scripts/uiTeamScaleHelpers.mjs";

const passing = {
  uiAllVisibleMs: 5_000,
  started: 3,
  replied: 3,
  leadReportedAtMs: 9_000,
  sqliteLockErrorDelta: 0,
  maxFrameGapMs: FRAME_GAP_LIMIT_MS,
  maxEchoMs: ECHO_LATENCY_LIMIT_MS,
  apiP95Ms: 40,
};

describe("ui team scale helpers", () => {
  it("computes nearest-rank percentiles", () => {
    expect(percentile([], 0.95)).toBeNull();
    expect(percentile([5, 1, 3, 2, 4], 0.95)).toBe(5);
    expect(percentile([5, 1, 3, 2, 4], 0.5)).toBe(3);
  });

  it("zero-pads worker tokens and asks for the batch count", () => {
    expect(workerToken("TAG", 7)).toBe("TAG_W0007");
    const prompt = buildLeadPrompt({
      workers: SPAWN_BATCH_MAX + 1,
      runTag: "TAG",
      workspace: "shared",
    });
    expect(prompt).toContain("Use exactly 2 call(s)");
    expect(prompt).toContain(
      `call 2: agents ${SPAWN_BATCH_MAX + 1} to ${SPAWN_BATCH_MAX + 1}`,
    );
    expect(prompt).toContain("TAG_ALL_REPLIED");
  });

  it("splits a team into spawn calls and times each call", () => {
    expect(spawnCalls(128)).toEqual([
      { call: 1, from: 1, to: 64 },
      { call: 2, from: 65, to: 128 },
    ]);
    const rows = [
      { name: "1", firstSeenAtMs: 5, startedAtMs: 6, completedAtMs: 9 },
      {
        name: "acceptance-0064",
        firstSeenAtMs: 7,
        startedAtMs: null,
        completedAtMs: null,
      },
      {
        name: "lead",
        firstSeenAtMs: 8,
        startedAtMs: null,
        completedAtMs: null,
      },
      { name: "65", firstSeenAtMs: 50, startedAtMs: 51, completedAtMs: 60 },
    ];
    expect(spawnBatchTimings(rows, 128)).toEqual([
      {
        call: 1,
        from: 1,
        to: 64,
        created: 2,
        firstSeenAtMs: 5,
        lastSeenAtMs: 7,
        firstStartedAtMs: 6,
        lastCompletedAtMs: 9,
        replied: 1,
      },
      {
        call: 2,
        from: 65,
        to: 128,
        created: 1,
        firstSeenAtMs: 50,
        lastSeenAtMs: 50,
        firstStartedAtMs: 51,
        lastCompletedAtMs: 60,
        replied: 1,
      },
    ]);
  });

  it("tracks start, token reply and lead report per worker", () => {
    const tracker = createTeamTracker("lead", "TAG", 2);
    tracker.observe({ id: "w1", rootId: "lead", status: "queued" }, 10);
    tracker.observe({ id: "w1", rootId: "lead", inFlight: true }, 20);
    tracker.observe({ id: "w1", rootId: "lead", lastAnswer: "TAG_W0001" }, 30);
    tracker.observe(
      { id: "other", rootId: "elsewhere", lastAnswer: "TAG_W0002" },
      31,
    );
    tracker.observe(
      {
        id: "lead",
        lastAnswer: "...",
        overview: { result: "TAG_ALL_REPLIED" },
      },
      35,
    );
    expect(tracker.leadReportedAtMs()).toBeNull();
    tracker.observe({ id: "lead", lastAnswer: "TAG_ALL_REPLIED" }, 40);
    const summary = tracker.summary();
    expect(summary).toMatchObject({
      visible: 1,
      started: 1,
      replied: 1,
      leadReportedAtMs: 40,
    });
    expect(summary.missingTokens).toEqual(["TAG_W0002"]);
    expect(tracker.workers()[0]).toMatchObject({
      firstSeenAtMs: 10,
      startedAtMs: 20,
      completedAtMs: 30,
    });
  });

  it("spreads the deadline over throttle waves per account", () => {
    expect(stageDeadlineMs(30, 3)).toBe(
      STAGE_DEADLINE_BASE_MS +
        30 * STAGE_DEADLINE_PER_WORKER_MS +
        THROTTLE_WAVE_MS,
    );
    expect(stageDeadlineMs(30, 1)).toBe(
      stageDeadlineMs(30, 3) + 2 * THROTTLE_WAVE_MS,
    );
    expect(stageDeadlineMs(ACCOUNT_CONCURRENT_REQUESTS)).toBe(
      STAGE_DEADLINE_BASE_MS +
        ACCOUNT_CONCURRENT_REQUESTS * STAGE_DEADLINE_PER_WORKER_MS +
        THROTTLE_WAVE_MS,
    );
  });

  it("reports account distribution and provider retries without failing", () => {
    const tracker = createTeamTracker("lead", "TAG", 3);
    const retrying = (at) => ({ phase: "retrying", at });
    tracker.observe({ id: "w1", rootId: "lead", accountKey: "b" }, 0);
    tracker.observe({ id: "w2", rootId: "lead", accountKey: "a" }, 0);
    tracker.observe({ id: "w3", rootId: "lead", accountKey: "a" }, 0);
    tracker.observe(
      { id: "w1", rootId: "lead", nativeStatus: retrying(1) },
      100,
    );
    tracker.observe(
      { id: "w1", rootId: "lead", nativeStatus: retrying(2) },
      200,
    );
    tracker.observe({ id: "w1", rootId: "lead" }, 1_100);
    tracker.observe({ id: "lead", nativeStatus: retrying(3) }, 1_000);
    tracker.observe(
      { id: "w2", rootId: "lead", nativeStatus: retrying(4) },
      1_500,
    );
    expect(tracker.summary().accountDistribution).toEqual({ a: 2, b: 1 });
    expect(tracker.throttling(2_000)).toEqual({
      events: 4,
      agents: 3,
      waitMs: 1_000 + 1_000 + 500,
    });
    expect(evaluateCriteria(passing, 3).passed).toBe(true);
  });

  it("finds the first sample that reaches a count", () => {
    const samples = [
      { atMs: 1, n: 0 },
      { atMs: 2, n: 3 },
      { atMs: 3, n: 5 },
    ];
    expect(firstReachedMs(samples, (s) => s.n, 3)).toBe(2);
    expect(firstReachedMs(samples, (s) => s.n, 9)).toBeNull();
  });

  it("counts new processes in full and ignores exited ones for CPU", () => {
    const previous = new Map([
      [1, 100],
      [2, 50],
    ]);
    expect(
      cpuDeltaMs(previous, [
        { pid: 1, cpuMs: 160 },
        { pid: 3, cpuMs: 30 },
      ]),
    ).toBe(90);
    expect(cpuPercent(500, 1_000)).toBe(50);
    expect(cpuPercent(1, 0)).toBeNull();
  });

  it("reads the lock error counter only when it is a count", () => {
    expect(
      sqliteLockErrors({ sqliteContention: { lockError: { lockErrors: 2 } } }),
    ).toBe(2);
    expect(sqliteLockErrors({})).toBeNull();
  });

  it("fails closed on unobserved or exceeded metrics", () => {
    expect(evaluateCriteria(passing, 3).passed).toBe(true);
    const failed = evaluateCriteria(
      {
        ...passing,
        sqliteLockErrorDelta: null,
        maxEchoMs: ECHO_LATENCY_LIMIT_MS + 1,
        replied: 2,
        started: 2,
      },
      3,
    );
    expect(failed.failures).toEqual(
      expect.arrayContaining([
        "noSqliteLockErrors",
        "allWorkerTurnsStarted",
        "allAgentsReplied",
        `echoUnder${ECHO_LATENCY_LIMIT_MS}ms`,
      ]),
    );
  });

  it("parses and validates options", () => {
    expect(
      parseArgs(["--workers", "30", "--keep"], { project: "/p" }),
    ).toMatchObject({
      workers: 30,
      keep: true,
      lead: "codex-luna",
      project: "/p",
    });
    expect(() => parseArgs(["--workers", "513"], {})).toThrow(/--workers/);
    expect(() => parseArgs(["--lead", "claude-haiku"], {})).toThrow(/--lead/);
    expect(() => parseArgs(["--bogus"], {})).toThrow(/Unknown option/);
  });
});
