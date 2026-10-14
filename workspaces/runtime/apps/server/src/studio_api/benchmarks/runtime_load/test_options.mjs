import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_WORKERS_PER_TEAM,
  MAX_SYNTHETIC_CALLBACK_IDENTITIES,
  MAX_BENCHMARK_TEAMS,
  MAX_WORKERS_PER_TEAM,
  assertSyntheticCallbackBudget,
  estimateSyntheticCallbackIdentities,
  parseIdentityOptions,
} from "./options.mjs";

test("identity options retain defaults and bound the fixture to 2048 workers", () => {
  assert.deepEqual(parseIdentityOptions([]), {
    teams: MAX_BENCHMARK_TEAMS,
    workersPerTeam: DEFAULT_WORKERS_PER_TEAM,
    workerCount: MAX_BENCHMARK_TEAMS * DEFAULT_WORKERS_PER_TEAM,
  });
  assert.deepEqual(
    parseIdentityOptions([
      "--teams",
      String(MAX_BENCHMARK_TEAMS),
      "--workers-per-team",
      String(MAX_WORKERS_PER_TEAM),
    ]),
    {
      teams: MAX_BENCHMARK_TEAMS,
      workersPerTeam: MAX_WORKERS_PER_TEAM,
      workerCount: MAX_BENCHMARK_TEAMS * MAX_WORKERS_PER_TEAM,
    },
  );
  assert.deepEqual(
    parseIdentityOptions([
      "--teams",
      "1",
      "--workers-per-team",
      String(MAX_WORKERS_PER_TEAM),
    ]),
    {
      teams: 1,
      workersPerTeam: MAX_WORKERS_PER_TEAM,
      workerCount: MAX_WORKERS_PER_TEAM,
    },
  );
});

test("identity options reject values outside the independently bounded axes", () => {
  for (const args of [
    ["--teams", "0"],
    ["--teams", "9"],
    ["--workers-per-team", "1"],
    ["--workers-per-team", String(MAX_WORKERS_PER_TEAM + 1)],
    ["--workers-per-team", "not-a-number"],
  ]) {
    assert.throws(() => parseIdentityOptions(args));
  }
});

test("callback identity estimate matches fixture phase schedule and resource budget", () => {
  const defaultRun = estimateSyntheticCallbackIdentities({
    workerCount: 256,
    rounds: 1,
    offeredRate: 160,
    steadySeconds: 30,
  });
  assert.equal(defaultRun, 89_088);

  const expandedRun = estimateSyntheticCallbackIdentities({
    workerCount: 2048,
    rounds: 1,
    offeredRate: 160,
    steadySeconds: 30,
  });
  assert.equal(expandedRun, 175_104);
  assert(expandedRun <= MAX_SYNTHETIC_CALLBACK_IDENTITIES);

  assert.throws(
    () =>
      assertSyntheticCallbackBudget({
        workerCount: 2048,
        rounds: 8,
        offeredRate: 2000,
        steadySeconds: 60,
      }),
    /workload would retain .* synthetic callback identities/,
  );
});
