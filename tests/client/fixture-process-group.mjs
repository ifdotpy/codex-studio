import {
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const processGroupsSupported = process.platform !== "win32";

function fixturePid(childOrPid) {
  return typeof childOrPid === "number" ? childOrPid : childOrPid.pid;
}

function readProcessStartTime(pid) {
  if (process.platform !== "linux") return undefined;
  try {
    const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
    const fields = stat
      .slice(stat.lastIndexOf(")") + 2)
      .trim()
      .split(/\s+/);
    // The remaining fields start at stat field 3 (state); starttime is field 22.
    return fields[19];
  } catch (error) {
    if (/** @type {NodeJS.ErrnoException} */ (error).code === "ENOENT")
      return undefined;
    throw error;
  }
}

/** @param {NodeJS.Signals | number} [signal] */
export function signalFixtureProcessGroup(childOrPid, signal = "SIGTERM") {
  const pid = fixturePid(childOrPid);
  if (pid === undefined) return childOrPid.kill(signal);
  if (!processGroupsSupported) {
    if (typeof childOrPid === "number") {
      try {
        process.kill(pid, signal);
        return true;
      } catch (error) {
        if (/** @type {NodeJS.ErrnoException} */ (error).code === "ESRCH")
          return false;
        throw error;
      }
    }
    return childOrPid.kill(signal);
  }
  try {
    process.kill(-pid, signal);
    return true;
  } catch (error) {
    if (/** @type {NodeJS.ErrnoException} */ (error).code === "ESRCH")
      return false;
    throw error;
  }
}

export function fixtureProcessGroupExists(pid) {
  try {
    process.kill(processGroupsSupported ? -pid : pid, 0);
    return true;
  } catch (error) {
    if (/** @type {NodeJS.ErrnoException} */ (error).code === "ESRCH")
      return false;
    if (/** @type {NodeJS.ErrnoException} */ (error).code === "EPERM")
      return true;
    throw error;
  }
}

async function waitForGroupExit(pid, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (fixtureProcessGroupExists(pid) && Date.now() < deadline)
    await delay(25);
  return !fixtureProcessGroupExists(pid);
}

export async function stopFixtureProcessGroup(childOrPid, timeoutMs = 1_000) {
  const pid = fixturePid(childOrPid);
  if (pid === undefined) return;

  signalFixtureProcessGroup(childOrPid, "SIGTERM");
  if (await waitForGroupExit(pid, timeoutMs)) return;

  signalFixtureProcessGroup(childOrPid, "SIGKILL");
  if (!(await waitForGroupExit(pid, timeoutMs)))
    throw new Error(`Fixture process group ${pid} did not exit after SIGKILL`);
}

export function registerFixtureProcessGroup(registryDirectory, pid) {
  mkdirSync(registryDirectory, { recursive: true });
  writeFileSync(
    join(registryDirectory, `${pid}.json`),
    JSON.stringify({ pid, startTime: readProcessStartTime(pid) }),
  );
}

export function unregisterFixtureProcessGroup(registryDirectory, pid) {
  rmSync(join(registryDirectory, `${pid}.json`), { force: true });
}

export async function reapFixtureProcessGroups(registryDirectory) {
  let files = [];
  try {
    files = readdirSync(registryDirectory).filter((name) =>
      /^\d+\.json$/.test(name),
    );
  } catch (error) {
    if (/** @type {NodeJS.ErrnoException} */ (error).code === "ENOENT") return;
    throw error;
  }
  const errors = [];
  for (const file of files) {
    const registryPath = join(registryDirectory, file);
    try {
      const record = JSON.parse(readFileSync(registryPath, "utf8"));
      const pid = Number(file.slice(0, -5));
      const currentStartTime = readProcessStartTime(pid);
      if (
        record.pid !== pid ||
        (process.platform === "linux" &&
          (typeof record.startTime !== "string" ||
            (currentStartTime !== undefined &&
              record.startTime !== currentStartTime)))
      ) {
        unregisterFixtureProcessGroup(registryDirectory, pid);
        continue;
      }
      await stopFixtureProcessGroup(pid);
      unregisterFixtureProcessGroup(registryDirectory, pid);
    } catch (error) {
      errors.push(error);
    }
  }
  if (errors.length)
    throw new AggregateError(errors, "Failed to reap fixture process groups");
}
