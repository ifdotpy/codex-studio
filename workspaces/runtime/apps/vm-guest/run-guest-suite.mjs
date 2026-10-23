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
// The last line has the counts that the check facade reads: `Tests  N passed, F failed,
// S skipped`.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const SUITE_TIMEOUT_MS = 300_000;
const guest = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(guest, "../../../..");
const python = process.env.STUDIO_GUEST_PYTHON || "python3";

function fail(message) {
  console.error(`run-guest-suite: ${message}`);
  process.exit(1);
}

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

const base = process.platform === "darwin" ? "/private/tmp" : os.tmpdir();
const temporary = mkdtempSync(path.join(base, "g"));
let run;
try {
  run = spawnSync(
    python,
    ["-B", "-m", "unittest", "discover", "-s", guest, "-p", "test_*.py"],
    {
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
    },
  );
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
const output = run.stderr ?? "";
process.stderr.write(output);
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
process.exit(run.status === 0 && result?.[1] === "OK" ? 0 : 1);
