import { describe, expect, it, vi } from "vitest";
import {
  ECHO_LATENCY_LIMIT_MS,
  FRAME_GAP_LIMIT_MS,
  SPAWN_BATCH_MAX,
  buildLeadPrompt,
  configureNewChat,
  createEntityReader,
  createEntityFrameReader,
  openEntityStream,
  cleanupVerified,
  readTeamUiCounts,
  approvedModelUiValues,
  approvedStoredModel,
  assertCreatedTeamProfiles,
  OWNED_CLEANUP_VERIFIED_STATE,
  summaryText,
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
    expect(tracker.leadReports()).toEqual([]);
    tracker.observe({ id: "lead", lastAnswer: "TAG_ALL_REPLIED" }, 40);
    const summary = tracker.summary();
    expect(summary).toMatchObject({
      visible: 1,
      started: 1,
      replied: 1,
      // One of two workers replied: the report does not count yet.
      leadReportedAtMs: null,
    });
    expect(summary.missingTokens).toEqual(["TAG_W0002"]);
    expect(tracker.workers()[0]).toMatchObject({
      firstSeenAtMs: 10,
      startedAtMs: 20,
      completedAtMs: 30,
    });
  });

  it("counts only a lead report made after the last worker reply", () => {
    const tracker = createTeamTracker("lead", "TAG", 1);
    tracker.observe({ id: "lead", lastAnswer: "TAG_ALL_REPLIED" }, 5);
    tracker.observe({ id: "w1", rootId: "lead", inFlight: true }, 6);
    tracker.observe({ id: "lead", lastAnswer: "..." }, 7);
    tracker.observe({ id: "w1", rootId: "lead", lastAnswer: "TAG_W0001" }, 30);
    expect(tracker.summary()).toMatchObject({
      replied: 1,
      lastReplyMs: 30,
      leadReportedAtMs: null,
    });
    tracker.observe({ id: "lead", lastAnswer: "TAG_ALL_REPLIED" }, 41);
    expect(tracker.summary().leadReportedAtMs).toBe(41);
    expect(tracker.leadReports()).toEqual([5, 41]);
  });

  it("spreads the deadline over throttle waves per account", () => {
    expect(stageDeadlineMs(30, 3)).toBe(195_000);
    expect(stageDeadlineMs(30, 1)).toBe(315_000);
    expect(stageDeadlineMs(10)).toBe(185_000);
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

// A strict dialog driver rejects obsolete popovers and records user actions.
// Only controls belonging to the selected role/provider exist in this fixture.
function setupDialog({ fast = true, selectedAccount = "other" } = {}) {
  const actions = [];
  let role = "Lead";
  let provider = "Codex";
  const locate = (scope, kind, { name, selected } = {}) => {
    if (scope === "dialog") {
      const prefix = role === "Lead" ? "Main agent" : "Default subagent";
      const account =
        role === "Lead" ? "Main agent account" : "Subagent account";
      const names = {
        radiogroup: [
          "Agent role",
          `${account} provider`,
          `${prefix} reasoning`,
        ],
        group: [account],
        listbox: [`${prefix} model`],
        switch: ["Fast mode"],
      };
      expect(names[kind], `Unexpected control ${kind}:${name}`).toContain(name);
    }
    const path = `${scope}/${kind}:${name}`;
    const node = {
      getByRole: (kind, options) => locate(path, kind, options),
      locator: (selector) => ({
        click: async () => {
          const name = /^label\[for="([^"]+)"\]$/.exec(selector)?.[1];
          expect(name).toBeDefined();
          actions.push(name);
          if (
            scope === "dialog" &&
            kind === "radiogroup" &&
            ["Lead", "Workers"].includes(name)
          )
            role = name;
          else if (kind === "radiogroup" && ["Codex", "Claude"].includes(name))
            provider = name;
          else if (kind !== "radiogroup" || name !== "Low")
            throw new Error(`Unexpected segment ${path}/${name}`);
        },
      }),
      and: (selector) => {
        expect(selector).toMatch(/^\[data-(?:account-key|value)=/);
        return node;
      },
      first: () => node,
      getAttribute: async () => (kind === "radio" ? name : selectedAccount),
      count: async () => (fast ? 1 : 0),
      setChecked: async (checked) => actions.push(`Fast mode ${checked}`),
      click: async () => {
        if (kind === "button") {
          expect(name).toEqual(/^Select account /);
          actions.push(`${role} account ${provider}`);
        } else if (kind === "option") actions.push(`${role} model ${provider}`);
        else throw new Error(`Unexpected click ${path}`);
      },
      waitFor: async (options) => {
        if (kind === "option") expect(selected).toBe(true);
        else if (kind === "radio") {
          expect(name).toBeDefined();
          expect(options).toEqual({ state: "attached" });
        } else throw new Error(`Unexpected wait ${path}`);
      },
    };
    return node;
  };
  return {
    actions,
    locator: (selector) => selector,
    getByRole: (kind, options) => locate("dialog", kind, options),
  };
}

describe("current New chat setup", () => {
  const codex = {
    profile: { provider: "codex", fastMode: false },
    accountKey: "default",
    uiModel: "gpt-6-luna",
    effort: "low",
  };
  const claude = (model) => ({
    profile: { provider: "claude", fastMode: false },
    accountKey: "claude-local",
    uiModel: model,
    effort: "low",
  });

  it("configures Lead and Workers in place, including account, effort and Fast mode", async () => {
    const dialog = setupDialog();
    await configureNewChat(dialog, codex, codex);
    expect(dialog.actions).toEqual([
      "Lead",
      "Codex",
      "Lead account Codex",
      "Lead model Codex",
      "Low",
      "Fast mode false",
      "Workers",
      "Codex",
      "Workers account Codex",
      "Workers model Codex",
      "Low",
      "Fast mode false",
    ]);
  });

  it("switches both roles to Claude and accepts absent optional modes and a read-only selected account", async () => {
    const dialog = setupDialog({
      fast: false,
      selectedAccount: "claude-local",
    });
    await configureNewChat(dialog, claude("sonnet"), claude("haiku"));
    expect(dialog.actions).toEqual([
      "Lead",
      "Claude",
      "Lead model Claude",
      "Low",
      "Workers",
      "Claude",
      "Workers model Claude",
      "Low",
    ]);
  });
});

const resourceFrame = (documents) => ({
  protocol: 4,
  workspaceId: "workspace",
  epoch: "epoch",
  revision: 3,
  reason: "change",
  resources: [{ kind: "state" }],
  resourceVersions: [
    { revision: 3, entityChanges: { after: 1, through: 3, documents } },
  ],
});
const upsert = (collection, id, value) => ({
  collection,
  id,
  seq: 3,
  op: "upsert",
  value: { id, ...value },
});

it("merges current summary/detail streams for owned agents and observes input events and deletions", async () => {
  const feeds = new Map();
  const observed = [];
  const events = [];
  const close = vi.fn();
  const pull = createEntityReader(
    async (resource, receive) => {
      feeds.set(JSON.stringify(resource), receive);
      return { close };
    },
    (agent) => observed.push(agent),
    (event) => events.push(event),
  );
  await pull();
  const state = feeds.get(JSON.stringify({ kind: "state" }));
  state(
    "resources",
    resourceFrame([
      upsert("agentSummary", "foreign", { rootId: "other" }),
      upsert("agentSummary", "lead", {
        rootId: "lead",
        model: "gpt-6-luna",
        status: "running",
      }),
      upsert("event", "input", { agent: "lead", status: "pending" }),
    ]),
  );
  expect(observed).toEqual([]);
  pull.watch("lead");
  await pull();
  const lead = feeds.get(
    JSON.stringify({ kind: "agentDetail", agentId: "lead" }),
  );
  lead(
    "resources",
    resourceFrame([
      upsert("agentDetail", "lead", {
        effort: "low",
        fastMode: false,
        workerDefaults: { model: "gpt-6-luna" },
      }),
    ]),
  );
  expect(observed.at(-1)).toMatchObject({
    id: "lead",
    model: "gpt-6-luna",
    effort: "low",
    fastMode: false,
    status: "running",
  });
  state(
    "resources",
    resourceFrame([
      upsert("agentSummary", "worker", { rootId: "lead", status: "running" }),
    ]),
  );
  await pull();
  const worker = feeds.get(
    JSON.stringify({ kind: "agentDetail", agentId: "worker" }),
  );
  worker(
    "resources",
    resourceFrame([upsert("agentDetail", "worker", { lastAnswer: "TOKEN" })]),
  );
  expect(observed.at(-1)).toMatchObject({
    id: "worker",
    rootId: "lead",
    lastAnswer: "TOKEN",
  });
  state(
    "resources",
    resourceFrame([
      { collection: "agentSummary", id: "lead", seq: 4, op: "delete" },
      { collection: "event", id: "input", seq: 4, op: "delete" },
    ]),
  );
  expect(observed.at(-1)).toMatchObject({ id: "lead", deleted: true });
  expect(events).toEqual([
    { id: "input", agent: "lead", status: "pending" },
    { id: "input", deleted: true },
  ]);
  expect(feeds.size).toBe(3);
  await pull.close();
  expect(close).toHaveBeenCalledTimes(3);
});

it("assembles the server's multipart entity-transfer envelope before applying documents", () => {
  const accept = vi.fn();
  const receive = createEntityFrameReader(accept);
  const documents = [upsert("agentSummary", "lead", { model: "gpt-6-luna" })];
  const data = JSON.stringify({ after: 1, through: 3, documents });
  const frame = {
    protocol: 4,
    transferId: "transfer",
    pageIndex: 0,
    partCount: 2,
  };
  receive("entity-transfer", {
    ...frame,
    partIndex: 0,
    data: data.slice(0, 20),
  });
  expect(accept).not.toHaveBeenCalled();
  receive("entity-transfer", {
    ...frame,
    partIndex: 1,
    complete: true,
    data: data.slice(20),
  });
  expect(accept).toHaveBeenCalledWith(documents);
  expect(() =>
    receive("entity-transfer", { ...frame, partIndex: 1, data: "" }),
  ).toThrow(/out of order/);
});

it("reads split SSE bytes through the advertised protocol and closes its owned connection", async () => {
  const received = Promise.withResolvers();
  const failed = vi.fn();
  const documents = [upsert("agentDetail", "lead", { effort: "low" })];
  const wire = `event: resources\ndata: ${JSON.stringify(resourceFrame(documents))}\n\n`;
  const mock = vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(async (url, options) => {
      expect(url.searchParams.get("protocol")).toBe("4");
      expect(JSON.parse(url.searchParams.get("resources"))).toEqual([
        { kind: "agentDetail", agentId: "lead" },
      ]);
      return new Response(
        new ReadableStream({
          start(controller) {
            options.signal.addEventListener("abort", () =>
              controller.error(new Error("aborted")),
            );
            const bytes = new TextEncoder().encode(wire);
            controller.enqueue(bytes.slice(0, 19));
            controller.enqueue(bytes.slice(19));
          },
        }),
      );
    });
  try {
    const stream = await openEntityStream({
      origin: "http://localhost",
      token: "fixture",
      protocol: { protocolVersion: 4, streamEndpoint: "/api/sync/stream" },
      resource: { kind: "agentDetail", agentId: "lead" },
      timeoutMs: 1000,
      receive: (name, data) => received.resolve({ name, data }),
      failed,
    });
    expect(await received.promise).toEqual({
      name: "resources",
      data: resourceFrame(documents),
    });
    await stream.close();
    expect(failed).not.toHaveBeenCalled();
  } finally {
    mock.mockRestore();
  }
});

it("checks owned cleanup while retaining unrelated loaded thread counts only as evidence", () => {
  const clean = {
    remaining: 0,
    leakedProviderPids: [],
    leakedProcesses: [],
    loadedThreads: 20,
    baselineLoadedThreads: 10,
  };
  expect(cleanupVerified(clean)).toBe(true);
  expect(cleanupVerified({ ...clean, remaining: 1 })).toBe(false);
  expect(cleanupVerified({ ...clean, leakedProviderPids: [1] })).toBe(false);
  expect(cleanupVerified({ ...clean, leakedProcesses: [{ pid: 2 }] })).toBe(
    false,
  );
});

it("reads the current TeamSummary strong counts without summing All with its statuses", () => {
  const selectors = [];
  const document = {
    querySelector: (selector) => {
      selectors.push(selector);
      if (selector === '#team [data-team-count="all"] strong')
        return { textContent: "3" };
      if (selector === '#team [data-team-count="completed"] strong')
        return { textContent: "2" };
      if (selector === '#team .worker-group [aria-expanded="false"]') return {};
      return null;
    },
    querySelectorAll: (selector) =>
      selector === "#team [data-worker]" ? [{}] : [],
  };
  expect(readTeamUiCounts(document)).toMatchObject({
    teamCount: 3,
    workerCards: 3,
    teamPanelClosed: false,
  });
  expect(selectors).not.toContain("[data-team-count] dd");
});

it("accepts only catalogue aliases resolving to the approved paid-test model", () => {
  const candidate = { model: "haiku", resolvedModel: "claude-haiku-5-5" };
  expect(
    approvedModelUiValues(
      {
        data: [
          candidate,
          { model: "default", resolvedModel: "claude-haiku-5-5" },
          { model: "claude-haiku-5-5" },
          { model: "opus", resolvedModel: "claude-opus-5-5" },
        ],
      },
      candidate,
    ),
  ).toEqual(["haiku", "default", "claude-haiku-5-5"]);
});

it("stages several transfer pages until the final complete marker and rejects missing pages", () => {
  const accept = vi.fn();
  const receive = createEntityFrameReader(accept);
  const page = (index, after, through, complete) => ({
    transferId: "multi",
    pageIndex: index,
    partIndex: 0,
    partCount: 1,
    complete,
    data: JSON.stringify({
      after,
      through,
      documents: [upsert("agentSummary", `worker-${index}`, {})],
    }),
  });
  receive("entity-transfer", page(0, 0, 1, false));
  receive("entity-transfer", page(1, 1, 2, false));
  expect(accept).not.toHaveBeenCalled();
  expect(() => receive.assertComplete()).toThrow(/incomplete/);
  receive("entity-transfer", page(2, 2, 3, true));
  expect(accept).toHaveBeenCalledTimes(1);
  expect(accept.mock.calls[0][0]).toHaveLength(3);
  expect(() => receive.assertComplete()).not.toThrow();
  const missing = createEntityFrameReader(vi.fn());
  missing("entity-transfer", page(0, 0, 1, false));
  expect(() => missing("entity-transfer", page(2, 2, 3, true))).toThrow(
    /pages arrived out of order/,
  );
  expect(() => missing.assertComplete()).toThrow(/incomplete/);
});

it("cannot certify observations interrupted before a required transfer completes", async () => {
  let receive;
  let failed;
  const pull = createEntityReader(async (_resource, accept, onError) => {
    receive = accept;
    failed = onError;
    return { close: async () => {} };
  }, vi.fn());
  await pull();
  receive("entity-transfer", {
    transferId: "interrupted",
    pageIndex: 0,
    partIndex: 0,
    partCount: 1,
    complete: false,
    data: JSON.stringify({ after: 0, through: 1, documents: [] }),
  });
  expect(() => pull.assertComplete()).toThrow(/incomplete/);
  failed(new Error("stream interrupted"));
  expect(() => pull.assertComplete()).toThrow(/stream interrupted/);
  await pull.close();
});

it("states the unknown owned-thread cleanup boundary explicitly", () => {
  expect(OWNED_CLEANUP_VERIFIED_STATE).toBe(
    "agents and owned processes verified; owned loaded threads not verifiable (diagnostics expose counts only)",
  );
  const summary = summaryText({
    runTag: "fixture",
    result: { passed: true, failures: [] },
    options: { workers: 3 },
    metrics: {},
    cleanup: { state: OWNED_CLEANUP_VERIFIED_STATE },
  });
  expect(summary).toContain(OWNED_CLEANUP_VERIFIED_STATE);
  expect(summary).not.toMatch(/cleanup\s+verified/);
});

it("validates stored model aliases against only catalogue-proven approved identities", () => {
  const role = {
    model: "claude-sonnet-5-5",
    uiModelValues: ["sonnet", "claude-sonnet-5-5"],
  };
  expect(approvedStoredModel("sonnet", role)).toBe("claude-sonnet-5-5");
  expect(approvedStoredModel("claude-sonnet-5-5", role)).toBe(
    "claude-sonnet-5-5",
  );
  expect(() => approvedStoredModel("default", role)).toThrow(/approved/);
  expect(() => approvedStoredModel("opus", role)).toThrow(/approved/);
  expect(() => approvedStoredModel(undefined, role)).toThrow(/approved/);
});

it("reports the evidence-only queue baseline, end and delta in the summary", () => {
  const summary = summaryText({
    runTag: "fixture",
    result: { passed: true, failures: [] },
    options: { workers: 3 },
    metrics: {},
    audit: {
      failures: [],
      anomalies: [],
      queueGauges: [
        {
          path: "/queues/durableInputPending",
          before: 13,
          after: 14,
          delta: 1,
        },
      ],
    },
  });
  expect(summary).toContain("durableInputPending");
  expect(summary).toContain("13 / 14 / 1");
});

it("validates the actual post-create lead and worker defaults through proven aliases", () => {
  const lead = {
    model: "claude-sonnet-5-5",
    uiModelValues: ["sonnet", "claude-sonnet-5-5"],
    effort: "low",
    accountKey: "claude",
    profile: { fastMode: false },
  };
  const worker = {
    model: "claude-haiku-5-5",
    uiModelValues: ["haiku", "default", "claude-haiku-5-5"],
    effort: "low",
  };
  const entity = {
    id: "lead",
    model: "sonnet",
    accountKey: "claude",
    effort: "low",
    fastMode: false,
    workerDefaults: { model: "default", effort: "low" },
  };
  expect(() => assertCreatedTeamProfiles(entity, lead, worker)).not.toThrow();
  expect(() =>
    assertCreatedTeamProfiles(
      {
        ...entity,
        workerDefaults: { ...entity.workerDefaults, model: "haiku" },
      },
      lead,
      worker,
    ),
  ).not.toThrow();
  expect(() =>
    assertCreatedTeamProfiles({ ...entity, model: "opus" }, lead, worker),
  ).toThrow(/approved/);
  expect(() =>
    assertCreatedTeamProfiles(
      {
        ...entity,
        workerDefaults: { ...entity.workerDefaults, model: "opus" },
      },
      lead,
      worker,
    ),
  ).toThrow(/approved/);
  expect(() =>
    assertCreatedTeamProfiles({ ...entity, fastMode: true }, lead, worker),
  ).toThrow(/fastMode/);
});
