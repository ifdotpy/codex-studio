import type { ChildProcess } from "node:child_process";
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

function fixturePid(childOrPid: ChildProcess | number) {
  return typeof childOrPid === "number" ? childOrPid : childOrPid.pid;
}

function readProcessStartTime(pid: number) {
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
    if (
      (error instanceof Error && "code" in error ? error.code : undefined) ===
      "ENOENT"
    )
      return undefined;
    throw error;
  }
}

export function signalFixtureProcessGroup(
  childOrPid: ChildProcess | number,
  signal: NodeJS.Signals | number = "SIGTERM",
) {
  const pid = fixturePid(childOrPid);
  if (pid === undefined && typeof childOrPid !== "number")
    return childOrPid.kill(signal);
  if (pid === undefined) return false;
  if (!processGroupsSupported) {
    if (typeof childOrPid === "number") {
      try {
        process.kill(pid, signal);
        return true;
      } catch (error) {
        if (
          (error instanceof Error && "code" in error
            ? error.code
            : undefined) === "ESRCH"
        )
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
    if (
      (error instanceof Error && "code" in error ? error.code : undefined) ===
      "ESRCH"
    )
      return false;
    throw error;
  }
}

export function fixtureProcessGroupExists(pid: number) {
  try {
    process.kill(processGroupsSupported ? -pid : pid, 0);
    return true;
  } catch (error) {
    if (
      (error instanceof Error && "code" in error ? error.code : undefined) ===
      "ESRCH"
    )
      return false;
    if (
      (error instanceof Error && "code" in error ? error.code : undefined) ===
      "EPERM"
    )
      return true;
    throw error;
  }
}

async function waitForGroupExit(pid: number, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs;
  while (fixtureProcessGroupExists(pid) && Date.now() < deadline)
    await delay(25);
  return !fixtureProcessGroupExists(pid);
}

export async function stopFixtureProcessGroup(
  childOrPid: ChildProcess | number,
  timeoutMs = 1_000,
) {
  const pid = fixturePid(childOrPid);
  if (pid === undefined) return;

  for (const signal of ["SIGTERM", "SIGKILL"] as NodeJS.Signals[]) {
    let denied;
    try {
      signalFixtureProcessGroup(childOrPid, signal);
    } catch (error) {
      if (
        (error instanceof Error && "code" in error ? error.code : undefined) !==
        "EPERM"
      )
        throw error;
      // Darwin can return EPERM for a group containing only zombies until
      // their parent reaps them. Require ESRCH from the group probe before
      // accepting cleanup; a live inaccessible group must still fail.
      denied = error;
    }
    if (await waitForGroupExit(pid, timeoutMs)) return;
    if (denied) throw denied;
  }
  throw new Error(`Fixture process group ${pid} did not exit after SIGKILL`);
}

export function registerFixtureProcessGroup(
  registryDirectory: string,
  pid: number,
) {
  mkdirSync(registryDirectory, { recursive: true });
  writeFileSync(
    join(registryDirectory, `${pid}.json`),
    JSON.stringify({ pid, startTime: readProcessStartTime(pid) }),
  );
}

export function unregisterFixtureProcessGroup(
  registryDirectory: string,
  pid: number,
) {
  rmSync(join(registryDirectory, `${pid}.json`), { force: true });
}

export async function reapFixtureProcessGroups(registryDirectory: string) {
  let files = [];
  try {
    files = readdirSync(registryDirectory).filter((name) =>
      /^\d+\.json$/.test(name),
    );
  } catch (error) {
    if (
      (error instanceof Error && "code" in error ? error.code : undefined) ===
      "ENOENT"
    )
      return;
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
