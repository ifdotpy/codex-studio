#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";

const TEST_DIR = dirname(fileURLToPath(import.meta.url));
const BOARD_COMMAND = join(dirname(TEST_DIR), "scripts", "codex-board");
const ROOT = mkdtempSync(join(tmpdir(), "codex-agents-sandbox-"));
const WORKTREE = join(ROOT, "worktree");
const BOARD_STATE = join(ROOT, "board");
const BLOCKED = join(ROOT, "blocked");
mkdirSync(WORKTREE);
mkdirSync(BOARD_STATE);
mkdirSync(BLOCKED);

const proc = spawn(process.env.CODEX_BIN || "codex", ["app-server", "--listen", "stdio://"], {
  stdio: ["pipe", "pipe", "pipe"],
});
let nextId = 1;
const pending = new Map();
let passed = false;

function call(method, params = {}) {
  const id = nextId++;
  proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  return new Promise((resolveCall, rejectCall) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      rejectCall(new Error(`${method} timed out`));
    }, 15_000);
    pending.set(id, {
      resolveCall: (value) => {
        clearTimeout(timeout);
        resolveCall(value);
      },
      rejectCall: (error) => {
        clearTimeout(timeout);
        rejectCall(error);
      },
    });
  });
}

createInterface({ input: proc.stdout }).on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.id === undefined || !pending.has(message.id)) return;
  const request = pending.get(message.id);
  pending.delete(message.id);
  if (message.error) {
    request.rejectCall(new Error(message.error.message || JSON.stringify(message.error)));
  } else {
    request.resolveCall(message.result);
  }
});

proc.on("error", (error) => {
  for (const request of pending.values()) request.rejectCall(error);
  pending.clear();
});

const sandboxPolicy = {
  type: "workspaceWrite",
  writableRoots: [WORKTREE, BOARD_STATE],
  networkAccess: false,
  excludeSlashTmp: true,
  excludeTmpdirEnvVar: true,
};

try {
  await call("initialize", {
    clientInfo: { name: "codex-agents-sandbox-smoke", version: "1" },
    capabilities: { experimentalApi: true },
  });
  const allowed = await call("command/exec", {
    command: [BOARD_COMMAND, "claim", "sandbox-build", "smoke:run:worker"],
    cwd: WORKTREE,
    env: { CODEX_BOARD_STATE_DIR: BOARD_STATE },
    sandboxPolicy,
    timeoutMs: 10_000,
  });
  assert.equal(allowed.exitCode, 0, allowed.stderr);
  assert.match(allowed.stdout, /CLAIMED sandbox-build/);
  assert.match(readFileSync(join(BOARD_STATE, "codex-board.json"), "utf8"), /sandbox-build/);

  const deniedPath = join(BLOCKED, "denied");
  const denied = await call("command/exec", {
    command: ["/usr/bin/touch", deniedPath],
    cwd: WORKTREE,
    sandboxPolicy,
    timeoutMs: 10_000,
  });
  assert.notEqual(denied.exitCode, 0);
  assert.equal(existsSync(deniedPath), false);
  passed = true;
  console.log("sandbox smoke: PASS");
} finally {
  proc.kill("SIGTERM");
  if (passed) rmSync(ROOT, { recursive: true, force: true });
  else console.error(`sandbox state kept at ${ROOT}`);
}
