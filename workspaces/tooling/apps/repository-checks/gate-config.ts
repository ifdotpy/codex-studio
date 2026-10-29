import { fileURLToPath } from "node:url";

// Maximum Cargo compilation jobs per gate, a job count (including nested builders).
export const CARGO_BUILD_JOBS = 4;
// Grace for an owned process group to exit after TERM before KILL, milliseconds.
export const PROCESS_STOP_GRACE_MS = 500;
// Hard command deadline for ordinary gate nodes, milliseconds (browser has its own cap too).
export const GATE_COMMAND_TIMEOUT_MS = 1_800_000;
// Machine-wide lock for the full gates (test-fast, test-comprehensive): a folder with a
// pid file inside. The wrapper scripts in studio-ops use the same path. Purpose: two full
// gates on one machine push the load over 120 and both fail on timeouts.
export const GATE_LOCK_PATH = "/tmp/studio-gate.lock";
// Interval between progress lines while a gate waits for the lock, milliseconds.
export const GATE_LOCK_NOTICE_MS = 60_000;
// Interval between lock checks while a gate waits, milliseconds.
export const GATE_LOCK_POLL_MS = 1_000;
// Environment variable that sets the maximum wait for the lock, in seconds. Unset: wait forever.
export const GATE_LOCK_MAX_WAIT_ENV = "STUDIO_GATE_LOCK_MAX_WAIT_S";

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
