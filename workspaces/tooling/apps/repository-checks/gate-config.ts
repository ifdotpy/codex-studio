import { fileURLToPath } from "node:url";

// Maximum Cargo compilation jobs per gate, a job count (including nested builders).
export const CARGO_BUILD_JOBS = 4;
// Grace for an owned process group to exit after TERM before KILL, milliseconds.
export const PROCESS_STOP_GRACE_MS = 500;
// Hard command deadline for ordinary gate nodes, milliseconds (browser has its own cap too).
export const GATE_COMMAND_TIMEOUT_MS = 1_800_000;

// Tests inherit host tools, but agent session settings must come from their fixtures.
export function scrubAgentEnvironment<T extends NodeJS.ProcessEnv>(
  environment: T,
): T {
  for (const key of Object.keys(environment))
    if (/^(CLAUDE|STUDIO_|AI_AGENT$)/.test(key)) delete environment[key];
  return environment;
}

// Just and CI consume this owner instead of copying the compilation budget.
if (process.argv[1] === fileURLToPath(import.meta.url))
  process.stdout.write(`${CARGO_BUILD_JOBS}\n`);
