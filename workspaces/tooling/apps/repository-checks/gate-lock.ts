import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import {
  GATE_LOCK_MAX_WAIT_ENV,
  GATE_LOCK_NOTICE_MS,
  GATE_LOCK_PATH,
  GATE_LOCK_POLL_MS,
} from "./gate-config.ts";

export interface GateLockOptions {
  lockPath?: string;
  pollMs?: number;
  noticeMs?: number;
  maxWaitMs?: number;
  write?: (text: string) => void;
}

function alive(pid: number) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

function ps(field: string, pid: number) {
  try {
    return execFileSync("ps", ["-o", `${field}=`, "-p", String(pid)], {
      encoding: "utf8",
    }).trim();
  } catch {
    return "";
  }
}

// True when pid is this process or one of its ancestors (the wrapper script, or an outer gate).
function isSelfOrAncestor(pid: number) {
  for (let next = process.pid; next > 1; next = Number(ps("ppid", next))) {
    if (next === pid) return true;
    if (!Number.isInteger(next)) return false;
  }
  return false;
}

function holderPid(lockPath: string) {
  try {
    return Number(readFileSync(path.join(lockPath, "pid"), "utf8").trim());
  } catch {
    return 0;
  }
}

// Take the machine lock, run the gate, release the lock on return, error, and exit.
export async function withGateLock<T>(
  run: () => Promise<T>,
  options: GateLockOptions = {},
): Promise<T> {
  const lockPath = options.lockPath ?? GATE_LOCK_PATH;
  const write = options.write ?? ((text) => process.stderr.write(text));
  const maxWait = Number(process.env[GATE_LOCK_MAX_WAIT_ENV]);
  const maxWaitMs =
    options.maxWaitMs ?? (maxWait > 0 ? maxWait * 1000 : Infinity);
  const pollMs = options.pollMs ?? GATE_LOCK_POLL_MS;
  const noticeMs = options.noticeMs ?? GATE_LOCK_NOTICE_MS;
  const started = Date.now();
  let lastNotice = started;
  let owned = false;
  for (;;) {
    try {
      mkdirSync(lockPath);
      writeFileSync(path.join(lockPath, "pid"), `${process.pid}\n`);
      owned = true;
      break;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
    }
    const holder = holderPid(lockPath);
    if (holder && isSelfOrAncestor(holder)) break;
    if (holder && !alive(holder)) {
      rmSync(lockPath, { recursive: true, force: true });
      continue;
    }
    const now = Date.now();
    if (now - started >= maxWaitMs)
      throw new Error(
        `Gate lock ${lockPath} still held by pid ${holder} after ${Math.round((now - started) / 1000)} s (${GATE_LOCK_MAX_WAIT_ENV})`,
      );
    if (now - lastNotice >= noticeMs) {
      lastNotice = now;
      write(
        `Waiting for the gate lock ${lockPath}: held by pid ${holder} (${ps("command", holder)})\n`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, pollMs));
  }
  const release = () => {
    if (owned && holderPid(lockPath) === process.pid)
      rmSync(lockPath, { recursive: true, force: true });
    owned = false;
  };
  process.once("exit", release);
  try {
    return await run();
  } finally {
    release();
    process.removeListener("exit", release);
  }
}
