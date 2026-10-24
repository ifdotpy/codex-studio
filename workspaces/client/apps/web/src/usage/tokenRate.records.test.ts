import { expect, it, vi } from "vitest";
import {
  TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS,
  TOKEN_RATE_MAX_ENTRIES,
  SYNC_PROTOCOL_VERSION,
} from "../generated/api";
import {
  createTokenRateCache,
  workerRateKey,
  type TokenRate,
} from "./tokenRate";

it("one token record drives footer and team without an agent projection, moves roots and clears removals", () => {
  const cache = createTokenRateCache();
  const record = {
    agentId: "worker",
    rootId: "first",
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 42,
    outputTokens: 84,
  } satisfies TokenRate;
  const footer: (TokenRate | null)[] = [],
    first: (TokenRate | null)[] = [],
    second: (TokenRate | null)[] = [];
  cache.subscribeTokenRate("worker", (value) => footer.push(value));
  cache.subscribeTokenRate(workerRateKey("first", "worker"), (value) =>
    first.push(value),
  );
  cache.subscribeTokenRate(workerRateKey("second", "worker"), (value) =>
    second.push(value),
  );
  const closeFirst = cache.watchTeamTokenRates("first");
  const closeSecond = cache.watchTeamTokenRates("second");
  expect(cache.receiveWorkspaceTokenRates({ records: [record] })).toBe(true);
  expect(footer.at(-1)).toEqual(record);
  expect(first.at(-1)).toEqual(record);
  expect(second.at(-1)).toBeNull();
  const moved = { ...record, rootId: "second" };
  expect(cache.receiveWorkspaceTokenRates({ records: [moved] })).toBe(true);
  expect(footer.at(-1)).toEqual(moved);
  expect(first.at(-1)).toBeNull();
  expect(second.at(-1)).toEqual(moved);
  expect(cache.receiveWorkspaceTokenRates({ records: [] })).toBe(true);
  expect(footer.at(-1)).toBeNull();
  expect(second.at(-1)).toBeNull();
  closeFirst();
  closeSecond();
});

it("rejects nonfinite, malformed, duplicate and oversized records atomically at the snapshot boundary", async () => {
  const { TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES } =
    await import("../generated/api");
  const cache = createTokenRateCache();
  const record = {
    agentId: "agent",
    rootId: "root",
    turnId: "turn",
    active: false,
    estimated: false,
    rate: 3,
    outputTokens: 6,
  };
  expect(cache.receiveWorkspaceTokenRates({ records: [record] })).toBe(true);
  for (const field of ["agentId", "rootId", "turnId"] as const) {
    for (const value of [
      "",
      "💠".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS + 1),
      null,
    ])
      expect(
        cache.receiveWorkspaceTokenRates({
          records: [{ ...record, [field]: value }],
        }),
      ).toBe(false);
  }
  for (const field of ["rate", "outputTokens"] as const)
    for (const value of [Infinity, -Infinity, NaN, -1, null, "3"])
      expect(
        cache.receiveWorkspaceTokenRates({
          records: [{ ...record, [field]: value }],
        }),
      ).toBe(false);
  for (const invalid of [
    null,
    [],
    { records: {} },
    { records: [record, record] },
    { records: [{ ...record, active: 1 }] },
    { records: [{ ...record, estimated: 1 }] },
    { records: [{ ...record, extension: 1 }] },
  ])
    expect(cache.receiveWorkspaceTokenRates(invalid)).toBe(false);
  expect(cache.getTokenRate("agent")).toEqual(record);
  const id = "💠".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS);
  const atLimit = Array.from(
    { length: TOKEN_RATE_MAX_ENTRIES },
    (_, index) => ({
      ...record,
      agentId: String(index),
      rootId: id,
      turnId: id,
    }),
  );
  expect(cache.receiveWorkspaceTokenRates({ records: atLimit })).toBe(true);
  expect(
    cache.receiveWorkspaceTokenRates({ records: [...atLimit, record] }),
  ).toBe(false);
  expect(cache.getTokenRate("0")).toEqual(atLimit[0]);
});

it("applies token-rate deltas atomically, preserves quiet records, and replaces on reconnect", () => {
  const cache = createTokenRateCache();
  const record: TokenRate = {
    agentId: "active",
    rootId: "first",
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 1,
    outputTokens: 10,
  };
  const quiet = { ...record, agentId: "quiet", rate: 0 };
  const gone = { ...record, agentId: "gone" };
  const other = { ...record, agentId: "other" };
  const frame = (
    snapshot: boolean,
    records: TokenRate[],
    removed: string[] = [],
  ) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
    snapshot,
    records,
    removed,
  });
  expect(
    cache.receiveResourceTokenRates(frame(true, [record, quiet, gone, other])),
  ).toBe(true);
  const observations: unknown[] = [];
  cache.subscribeTokenRate("active", () =>
    observations.push([
      cache.getTokenRate("quiet"),
      cache.getTokenRate("gone"),
      cache.getWorkspaceTeamTokenRate("first", "active"),
      cache.getWorkspaceTeamTokenRate("second", "active"),
      cache.getTokenRate("other"),
    ]),
  );
  observations.length = 0;
  const moved = { ...record, rootId: "second", rate: 5 };
  const updatedOther = { ...other, rate: 8 };
  expect(
    cache.receiveResourceTokenRates(
      frame(false, [moved, updatedOther], ["gone"]),
    ),
  ).toBe(true);
  expect(observations).toEqual([[quiet, null, null, moved, updatedOther]]);
  expect(cache.getTokenRate("quiet")).toEqual(quiet);
  expect(cache.receiveResourceTokenRates(frame(true, [quiet]))).toBe(true);
  expect(cache.getTokenRate("active")).toBeNull();
});

it("rejects malformed delta metadata and aggregate overflow without partially applying", () => {
  const cache = createTokenRateCache();
  const record: TokenRate = {
    agentId: "agent",
    rootId: "root",
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 1,
    outputTokens: 10,
  };
  const frame = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
    snapshot: true,
    records: [record],
    removed: [] as string[],
  };
  expect(cache.receiveResourceTokenRates(frame)).toBe(true);
  for (const invalid of [
    { ...frame, snapshot: undefined },
    { ...frame, removed: undefined },
    { ...frame, snapshot: false, removed: ["agent", "agent"] },
    { ...frame, snapshot: false, removed: ["agent"] },
    { ...frame, snapshot: false, records: [], removed: [""] },
    { ...frame, removed: ["other"] },
    {
      ...frame,
      snapshot: false,
      records: [],
      removed: ["a".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS + 1)],
    },
    {
      ...frame,
      snapshot: false,
      records: [],
      removed: Array.from(
        { length: TOKEN_RATE_MAX_ENTRIES + 1 },
        (_, index) => `removed-${index}`,
      ),
    },
  ]) {
    expect(cache.receiveResourceTokenRates(invalid as typeof frame)).toBe(
      false,
    );
    expect(cache.getTokenRate("agent")).toEqual(record);
  }
  const records = Array.from(
    { length: TOKEN_RATE_MAX_ENTRIES },
    (_, index) => ({ ...record, agentId: `agent-${index}` }),
  );
  expect(cache.receiveResourceTokenRates({ ...frame, records })).toBe(true);
  expect(cache.receiveResourceTokenRates({ ...frame, snapshot: false })).toBe(
    false,
  );
  expect(cache.getTokenRate("agent")).toBeNull();
  expect(
    cache.receiveResourceTokenRates({
      ...frame,
      snapshot: false,
      removed: ["agent-0"],
    }),
  ).toBe(true);
  expect(cache.getTokenRate("agent-0")).toBeNull();
  expect(cache.getTokenRate("agent")).toEqual(record);
});

it("requires a token baseline for each epoch before applying deltas", () => {
  const cache = createTokenRateCache();
  const record = {
    agentId: "quiet",
    rootId: "root",
    turnId: "turn",
    active: false,
    estimated: false,
    rate: 0,
    outputTokens: 10,
  };
  const frame = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "one",
    revision: 7,
    snapshot: false,
    records: [record],
    removed: [],
  };
  expect(cache.receiveResourceTokenRates(frame)).toBe(false);
  expect(cache.getTokenRate("quiet")).toBeNull();
  expect(cache.receiveResourceTokenRates({ ...frame, snapshot: true })).toBe(
    true,
  );
  expect(cache.receiveResourceTokenRates({ ...frame, epoch: "two" })).toBe(
    false,
  );
  expect(cache.getTokenRate("quiet")).toEqual(record);
  expect(
    cache.receiveResourceTokenRates({ ...frame, epoch: "two", snapshot: true }),
  ).toBe(true);
  expect(
    cache.receiveResourceTokenRates({
      ...frame,
      epoch: "two",
      records: [],
      removed: ["quiet"],
    }),
  ).toBe(true);
  cache.clearWorkspaceTokenRates();
  expect(cache.receiveResourceTokenRates({ ...frame, epoch: "two" })).toBe(
    false,
  );
});

it("notifies only delta agents and their previous and current team memberships", () => {
  const cache = createTokenRateCache();
  const record: TokenRate = {
    agentId: "active",
    rootId: "first",
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 1,
    outputTokens: 10,
  };
  const quiet = { ...record, agentId: "quiet" };
  const removed = { ...record, agentId: "gone" };
  const frame = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
    snapshot: true,
    records: [record, quiet, removed],
    removed: [] as string[],
  };
  cache.receiveResourceTokenRates(frame);
  const closeFirst = cache.watchTeamTokenRates("first");
  const closeSecond = cache.watchTeamTokenRates("second");
  const ids = [
    "active",
    "quiet",
    "gone",
    "missing",
    workerRateKey("first", "active"),
    workerRateKey("second", "active"),
    workerRateKey("first", "quiet"),
    workerRateKey("first", "gone"),
  ];
  const callbacks = ids.map((id) => {
    const callback = vi.fn();
    cache.subscribeTokenRate(id, callback);
    callback.mockClear();
    return callback;
  });
  const moved = { ...record, rootId: "second", rate: 5 };
  expect(
    cache.receiveResourceTokenRates({
      ...frame,
      snapshot: false,
      records: [moved],
      removed: ["gone"],
    }),
  ).toBe(true);
  expect(callbacks.map((callback) => callback.mock.calls)).toEqual([
    [[moved]],
    [],
    [[null]],
    [],
    [[null]],
    [[moved]],
    [],
    [[null]],
  ]);
  expect(cache.getTokenRate("quiet")).toBe(quiet);
  callbacks.forEach((callback) => callback.mockClear());
  expect(
    cache.receiveResourceTokenRates({
      ...frame,
      snapshot: false,
      records: [],
      removed: [],
    }),
  ).toBe(true);
  expect(callbacks.every((callback) => !callback.mock.calls.length)).toBe(true);
  closeFirst();
  closeSecond();
});
