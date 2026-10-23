import { expect, it } from "vitest";
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
