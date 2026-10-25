#!/usr/bin/env node
// Run the VM guest Python suite (docs/testing.md, "Fast, comprehensive, and manual checks").
// The guest runs on Linux with Python 3.12. This runner also runs the suite on a macOS host:
//
// - Python: `STUDIO_GUEST_PYTHON`, else `python3` on PATH; it must be 3.11 or newer.
// - Supervisor: `STUDIO_SUPERVISOR_BINARY`, else `codex-process-supervisor` of the Cargo debug
//   target, which `cargo build -p studio-process-supervisor` builds before this runner.
// - Temporary root: a macOS Unix socket path has at most 104 bytes, and the guest's control
//   sockets (`<state>/providers/<uuid>/control.sock`) do not fit below the per-user macOS
//   temporary folder. On macOS the suite therefore gets its own short folder in `/private/tmp`
//   (the real path of `/tmp`, because the tests resolve their folders); on Linux it gets one
//   in the platform temporary folder. The runner removes the folder at the end.
//
// - Leaked processes: every process that the suite starts (the guest supervisor, the
//   process supervisor and their commands) names the temporary root in its arguments or works
//   in it. After the suite ends, also on a failure or a timeout, the runner waits a short
//   time, then stops each such process (TERM, then KILL) and fails the run, because a test
//   must stop what it starts.
//
// The last line has the counts that the check facade reads: `Tests  N passed, F failed,
// S skipped`.
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readdirSync,
  readlinkSync,
  realpathSync,
  rmSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const SUITE_TIMEOUT_MS = 300_000;
/// Time for processes of the suite to end by themselves before they count as leaked.
const EXIT_GRACE_MS = 5_000;
/// Time between TERM and KILL for a leaked process.
const STOP_GRACE_MS = 5_000;
const PROBE_TIMEOUT_MS = 10_000;
const POLL_MS = 100;
const guest = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(guest, "../../../..");

/** Base directory for owned suite folders; Mac socket paths require a short base. */
export function suiteTemporaryBase() {
  return process.platform === "darwin" ? "/private/tmp" : os.tmpdir();
}

function fail(message) {
  console.error(`run-guest-suite: ${message}`);
  process.exit(1);
}

function within(file, folders) {
  return folders.some(
    (folder) => file === folder || file.startsWith(folder + path.sep),
  );
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

function pause(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

/** The working folders of selected PIDs, or all processes of this user. */
function workingFolders(pids) {
  const folders = new Map();
  if (existsSync("/proc/self/cwd")) {
    for (const name of pids ?? readdirSync("/proc")) {
      if (!/^\d+$/.test(name)) continue;
      try {
        folders.set(+name, readlinkSync(`/proc/${name}/cwd`));
      } catch {
        // Another user's process, or one that already ended.
      }
    }
    return folders;
  }
  const listed = spawnSync(
    "lsof",
    [
      "-a",
      "-d",
      "cwd",
      "-u",
      String(os.userInfo().uid),
      ...(pids === undefined ? [] : ["-p", pids.join(",")]),
      "-F",
      "pn",
    ],
    { encoding: "utf8", timeout: PROBE_TIMEOUT_MS },
  );
  if (listed.error || listed.status !== 0 || listed.signal)
    throw new Error(
      `lsof did not list the selected working folders: ${listed.error?.message || listed.stderr || listed.signal || `exit ${listed.status}`}`,
    );
  let pid = null;
  for (const line of (listed.stdout ?? "").split("\n")) {
    if (line.startsWith("p")) pid = +line.slice(1);
    else if (line.startsWith("n") && pid !== null)
      folders.set(pid, line.slice(1));
  }
  return folders;
}

/**
 * The processes that belong to a suite run in `folder`: they name the folder in their
 * arguments, or they work in it. Optional PIDs bound both probes to a caller's
 * known process set. The ordinary suite omits them to inspect this operator's processes.
 */
export function suiteProcesses(folder, { pids } = {}) {
  if (pids !== undefined) {
    if (
      !Array.isArray(pids) ||
      [...pids].some((pid) => !Number.isSafeInteger(pid) || pid <= 0)
    )
      throw new Error("PID selection must contain only positive integers");
    pids = [...new Set(pids)];
    if (pids.length === 0) return [];
  }
  const folders = [...new Set([folder, realpathSync(folder)])];
  const listed = spawnSync(
    "ps",
    [
      ...(pids === undefined
        ? ["-u", String(os.userInfo().uid)]
        : ["-p", pids.join(",")]),
      "-ww",
      "-o",
      "pid=",
      "-o",
      "args=",
    ],
    {
      encoding: "utf8",
      timeout: PROBE_TIMEOUT_MS,
    },
  );
  // ps exits 1 when none of the selected processes still exist. Errors,
  // timeouts, diagnostics, and partial output must not become an empty result.
  if (
    pids !== undefined &&
    !listed.error &&
    listed.status === 1 &&
    !listed.stdout?.trim() &&
    !listed.stderr?.trim()
  )
    return [];
  if (listed.error || listed.status !== 0)
    throw new Error(
      `ps did not list the processes: ${listed.error?.message ?? listed.stderr}`,
    );
  const rows = listed.stdout
    .split("\n")
    .map((line) => line.match(/^\s*(\d+)\s+(.*)$/))
    .filter(Boolean);
  if (
    pids !== undefined &&
    (rows.length === 0 || rows.some((row) => !pids.includes(+row[1])))
  )
    throw new Error("ps did not list only confirmed selected processes");
  // lsof reports failure for a requested PID that has ended, even when
  // another selected PID remains. Probe only the PIDs that ps confirms.
  const cwd = workingFolders(
    pids === undefined ? undefined : rows.map((row) => +row[1]),
  );
  const found = [];
  for (const match of rows) {
    const pid = +match[1];
    if (pid === process.pid) continue;
    const command = match[2];
    if (
      folders.some((name) => command.includes(name)) ||
      within(cwd.get(pid) ?? "", folders)
    )
      found.push({ pid, command });
  }
  return found;
}

/** Wait until every process has ended or the time is over; returns the processes alive. */
function waitForExit(processes, milliseconds) {
  const deadline = Date.now() + milliseconds;
  let living = processes.filter(({ pid }) => alive(pid));
  while (living.length && Date.now() < deadline) {
    pause(POLL_MS);
    living = living.filter(({ pid }) => alive(pid));
  }
  return living;
}

/**
 * Stop the processes of a suite run in `folder` that are still alive after a short grace:
 * TERM, then KILL. Returns the processes that had to be stopped.
 */
export function stopSuiteProcesses(
  folder,
  { exitGrace = EXIT_GRACE_MS, stopGrace = STOP_GRACE_MS, pids } = {},
) {
  const leaked = waitForExit(suiteProcesses(folder, { pids }), exitGrace);
  for (const { pid } of leaked) {
    try {
      process.kill(pid, "SIGTERM");
    } catch {
      // It ended in the meantime.
    }
  }
  for (const { pid } of waitForExit(leaked, stopGrace)) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // It ended in the meantime.
    }
  }
  return leaked;
}

function main() {
  const python = process.env.STUDIO_GUEST_PYTHON || "python3";
  const version = spawnSync(
    python,
    ["-c", "import sys; print(int(sys.version_info >= (3, 11)))"],
    { encoding: "utf8" },
  );
  if (version.error || version.stdout.trim() !== "1")
    fail(
      `${python} is missing or older than Python 3.11; set STUDIO_GUEST_PYTHON`,
    );

  const supervisor =
    process.env.STUDIO_SUPERVISOR_BINARY ||
    path.join(
      process.env.CARGO_TARGET_DIR || path.join(root, "target"),
      "debug",
      "codex-process-supervisor",
    );
  if (!existsSync(supervisor))
    fail(
      `${supervisor} is missing; run cargo build -p studio-process-supervisor --locked first`,
    );

  const temporary = mkdtempSync(path.join(suiteTemporaryBase(), "g"));
  let run;
  let leaked = [];
  try {
    run = spawnSync(python, ["-B", path.join(guest, "guest_suite.py")], {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        TMPDIR: temporary,
        STUDIO_SUPERVISOR_BINARY: supervisor,
        // The retained guest must not reach a removed backend Python.
        CODEX_AGENTS_PYTHON: "/nonexistent",
      },
      stdio: ["ignore", "inherit", "pipe"],
      timeout: SUITE_TIMEOUT_MS,
    });
  } finally {
    process.stderr.write(run?.stderr ?? "");
    console.error(
      `run-guest-suite: Python result: ${JSON.stringify({
        status: run?.status ?? null,
        signal: run?.signal ?? null,
        error: run?.error?.message ?? null,
      })}`,
    );
    leaked = stopSuiteProcesses(temporary);
    rmSync(temporary, { recursive: true, force: true });
  }
  const output = run.stderr ?? "";
  for (const { pid, command } of leaked)
    console.error(
      `run-guest-suite: stopped a process that the suite left running: ${pid} ${command}`,
    );
  if (run.error) fail(`the suite did not finish: ${run.error.message}`);
  const ran = +(output.match(/^Ran (\d+) tests? in /m)?.[1] ?? NaN);
  if (!Number.isInteger(ran)) fail("unittest printed no summary");
  const result = output.match(/^(OK|FAILED)(?: \((.*)\))?$/m);
  const field = (name) =>
    +(result?.[2]?.match(new RegExp(`${name}=(\\d+)`))?.[1] ?? 0);
  const skipped = field("skipped");
  const failed =
    field("failures") + field("errors") + field("unexpected successes");
  console.log(
    `Tests  ${ran - skipped - failed} passed, ${failed} failed, ${skipped} skipped`,
  );
  if (leaked.length)
    fail(`${leaked.length} processes of the suite outlived it`);
  process.exit(run.status === 0 && result?.[1] === "OK" ? 0 : 1);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  main();
