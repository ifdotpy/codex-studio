import assert from "node:assert/strict";

export const MAX_BENCHMARK_TEAMS = 8;
export const DEFAULT_WORKERS_PER_TEAM = 32;
export const MAX_WORKERS_PER_TEAM = 256;
export const MAX_SYNTHETIC_CALLBACK_IDENTITIES = 200_000;
export const SYNTHETIC_CALLBACK_IDENTITIES_PER_TURN = 16;

export function parseIdentityOptions(args) {
  const teamsAt = args.indexOf("--teams");
  const teams = teamsAt >= 0 ? Number(args[teamsAt + 1]) : MAX_BENCHMARK_TEAMS;
  const workersAt = args.indexOf("--workers-per-team");
  const workersPerTeam =
    workersAt >= 0 ? Number(args[workersAt + 1]) : DEFAULT_WORKERS_PER_TEAM;

  assert(
    Number.isInteger(teams) && teams >= 1 && teams <= MAX_BENCHMARK_TEAMS,
    `--teams must be 1..${MAX_BENCHMARK_TEAMS}`,
  );
  assert(
    Number.isInteger(workersPerTeam) &&
      workersPerTeam >= 2 &&
      workersPerTeam <= MAX_WORKERS_PER_TEAM,
    `--workers-per-team must be 2..${MAX_WORKERS_PER_TEAM}`,
  );

  return { teams, workersPerTeam, workerCount: teams * workersPerTeam };
}

export function estimateSyntheticCallbackIdentities({
  workerCount,
  rounds,
  offeredRate,
  steadySeconds,
}) {
  const phaseBaseTurns = workerCount * rounds;
  const steadyTurns = Math.max(
    phaseBaseTurns,
    Math.ceil(offeredRate * steadySeconds),
  );
  const totalTurns = phaseBaseTurns * 3 + steadyTurns;
  return totalTurns * SYNTHETIC_CALLBACK_IDENTITIES_PER_TURN;
}

export function assertSyntheticCallbackBudget(options) {
  const estimate = estimateSyntheticCallbackIdentities(options);
  assert(
    Number.isSafeInteger(estimate) &&
      estimate <= MAX_SYNTHETIC_CALLBACK_IDENTITIES,
    `workload would retain ${estimate} synthetic callback identities; budget is ${MAX_SYNTHETIC_CALLBACK_IDENTITIES}`,
  );
  return estimate;
}
