#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const TEST_DIR = dirname(fileURLToPath(import.meta.url));
const SKILL_DIR = dirname(TEST_DIR);
const SCRIPTS = join(SKILL_DIR, "scripts");
const ROOT = mkdtempSync(join(tmpdir(), "codex-agents-smoke-"));
const STATE = join(ROOT, "state");
mkdirSync(STATE);

let passed = false;

function environment(extra = {}) {
  return { ...process.env, CODEX_BOARD_STATE_DIR: "", CODEX_AGENTS_STATE_DIR: STATE, ...extra };
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    env: environment(options.env),
    timeout: options.timeout || 10_000,
  });
  const expected = options.status ?? 0;
  assert.equal(
    result.status,
    expected,
    `${command} ${args.join(" ")}\nstdout: ${result.stdout}\nstderr: ${result.stderr}`,
  );
  return result;
}

async function runAsync(command, args, options = {}) {
  const child = spawn(command, args, {
    env: environment(options.env),
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (data) => { stdout += data; });
  child.stderr.on("data", (data) => { stderr += data; });
  const status = await new Promise((resolveStatus, rejectStatus) => {
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      rejectStatus(new Error(`${command} timed out`));
    }, options.timeout || 10_000);
    child.on("error", rejectStatus);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolveStatus(code);
    });
  });
  const expected = options.status ?? 0;
  assert.equal(status, expected, `${command} ${args.join(" ")}\nstdout: ${stdout}\nstderr: ${stderr}`);
  return { status, stdout, stderr };
}

function runScript(name, args, options) {
  return run(join(SCRIPTS, name), args, options);
}

function writeJson(path, value) {
  writeFileSync(path, JSON.stringify(value, null, 2) + "\n");
}

const delay = (milliseconds) => new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));

async function waitFor(check, timeout = 5000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const value = check();
    if (value) return value;
    await delay(25);
  }
  throw new Error(`condition did not become true in ${timeout} ms`);
}

function readRpc(path) {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
}

function createFakeCodex(path) {
  writeFileSync(path, String.raw`#!/usr/bin/env node
import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";

let threadCount = 0;
const turnCounts = new Map();
const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
createInterface({ input: process.stdin }).on("line", (line) => {
  const request = JSON.parse(line);
  appendFileSync(process.env.FAKE_CODEX_LOG, JSON.stringify(request) + "\n");
  if (request.method === "initialize") return send({ jsonrpc: "2.0", id: request.id, result: {} });
  if (request.method === "model/list") {
    const second = request.params.cursor === "page-2";
    return send({
      jsonrpc: "2.0",
      id: request.id,
      result: {
        data: second ? [{
          id: "gpt-secondary",
          isDefault: false,
          supportedReasoningEfforts: [{ reasoningEffort: "low" }]
        }] : [{
          id: "gpt-5.6-luna",
          isDefault: true,
          supportedReasoningEfforts: [
            { reasoningEffort: "low" },
            { reasoningEffort: "max" }
          ]
        }],
        nextCursor: second ? null : "page-2"
      }
    });
  }
  if (request.method === "thread/start") {
    threadCount += 1;
    return send({
      jsonrpc: "2.0",
      id: request.id,
      result: { thread: { id: "thread-" + threadCount }, model: request.params.model }
    });
  }
  if (request.method === "thread/goal/set") return send({
    jsonrpc: "2.0",
    id: request.id,
    result: { goal: { threadId: request.params.threadId, status: request.params.status } }
  });
  if (request.method === "turn/start") {
    const turnCount = (turnCounts.get(request.params.threadId) || 0) + 1;
    turnCounts.set(request.params.threadId, turnCount);
    const turnId = "turn-" + request.params.threadId + "-" + turnCount;
    if (request.params.threadId === "thread-1") {
      // Force response and terminal notifications into one stdout chunk.
      process.stdout.write([
        { jsonrpc: "2.0", id: request.id, result: { turn: { id: turnId } } },
        { method: "turn/completed", params: { threadId: request.params.threadId,
          turn: { id: turnId, status: "completed", error: null } } },
        { method: "thread/goal/updated", params: { threadId: request.params.threadId,
          goal: { status: "complete", tokensUsed: 80 } } },
      ].map(JSON.stringify).join("\n") + "\n");
      return;
    }
    if (request.params.threadId === "thread-4") {
      process.stdout.write([
        { jsonrpc: "2.0", id: request.id, result: { turn: { id: turnId } } },
        { method: "turn/completed", params: { threadId: request.params.threadId,
          turn: { id: turnId, status: "completed", error: null } } },
        { method: "turn/started", params: { threadId: request.params.threadId,
          turn: { id: "turn-thread-4-2", status: "inProgress", error: null } } },
      ].map(JSON.stringify).join("\n") + "\n");
      return;
    }
    send({ jsonrpc: "2.0", id: request.id, result: { turn: { id: turnId } } });
    if (request.params.threadId === "thread-2") {
      setTimeout(() => send({
        jsonrpc: "2.0",
        method: "thread/goal/updated",
        params: {
          threadId: request.params.threadId,
          goal: { status: "budgetLimited", tokensUsed: 120 }
        }
      }), 20);
      setTimeout(() => send({
        jsonrpc: "2.0",
        method: "turn/completed",
        params: {
          threadId: request.params.threadId,
          turn: { id: turnId, status: "completed", error: null }
        }
      }), 40);
    }
    if (request.params.threadId === "thread-3") {
      setTimeout(() => send({
        jsonrpc: "2.0",
        method: "turn/completed",
        params: {
          threadId: request.params.threadId,
          turn: { id: turnId, status: "interrupted", error: null }
        }
      }), 40);
    }
    if (request.params.threadId === "thread-6") {
      setTimeout(() => send({
        jsonrpc: "2.0",
        method: "turn/completed",
        params: {
          threadId: request.params.threadId,
          turn: { id: turnId, status: "completed", error: null }
        }
      }), 20);
    }
    return;
  }
  if (request.method === "turn/steer") {
    if (request.params.threadId === "thread-5") {
      return send({
        jsonrpc: "2.0",
        id: request.id,
        error: { code: -32000, message: "steer rejected for smoke test" }
      });
    }
    return send({
      jsonrpc: "2.0",
      id: request.id,
      result: { turnId: request.params.expectedTurnId }
    });
  }
});
`);
  chmodSync(path, 0o755);
}

function createWorktrees() {
  const source = join(ROOT, "source");
  run("git", ["init", "-b", "main", source]);
  run("git", ["-C", source, "config", "user.email", "smoke@example.invalid"]);
  run("git", ["-C", source, "config", "user.name", "Smoke Test"]);
  writeFileSync(join(source, "README.md"), "smoke\n");
  run("git", ["-C", source, "add", "README.md"]);
  run("git", ["-C", source, "commit", "-m", "smoke baseline"]);

  const names = ["complete", "limited", "reviewer", "active", "steer-error", "waiting"];
  return names.map((name) => {
    const path = join(ROOT, `worker-${name}`);
    const branch = `codex/smoke-${name}`;
    run("git", ["-C", source, "worktree", "add", "-b", branch, path]);
    return { name, path, branch };
  });
}

async function testLauncher() {
  const fakeCodex = join(ROOT, "fake-codex.mjs");
  const rpcLog = join(ROOT, "rpc.jsonl");
  createFakeCodex(fakeCodex);

  const models = runScript("codex-models", [], {
    env: { CODEX_BIN: fakeCodex, FAKE_CODEX_LOG: rpcLog },
  });
  assert.match(models.stdout, /gpt-5\.6-luna/);
  assert.match(models.stdout, /low,max/);
  assert.match(models.stdout, /gpt-secondary/);

  const worktrees = createWorktrees();
  const workers = [
    { name: "worker-complete", role: "implementer", worktree: worktrees[0] },
    { name: "worker-limited", role: "implementer", worktree: worktrees[1] },
    { name: "worker-reviewer", role: "reviewer", worktree: worktrees[2] },
    { name: "worker-active", role: "implementer", worktree: worktrees[3] },
    { name: "worker-steer-error", role: "implementer", worktree: worktrees[4] },
    { name: "worker-waiting", role: "implementer", worktree: worktrees[5] },
  ];
  writeJson(join(STATE, "codex-tasks.smoke.json"), workers.map((worker) => ({
    name: worker.name,
    role: worker.role,
    cwd: worker.worktree.path,
    branch: worker.worktree.branch,
    objective: `Run the ${worker.name} smoke case.`,
    prompt: `Return the ${worker.name} smoke result.`,
  })));

  const launcher = spawn(join(SCRIPTS, "codex-swarm.mjs"), [], {
    env: environment({
      CODEX_BIN: fakeCodex,
      CODEX_WAVE: "smoke",
      CODEX_ORCHESTRATOR_ID: "fixture-orchestrator",
      CODEX_ORCHESTRATOR_NAME: "Fixture lead",
      CODEX_INBOX_POLL_MS: "50",
      CODEX_INBOX_RETRIES: "2",
      CODEX_INBOX_BACKOFF_MS: "50",
      CODEX_APPROVAL_POLICY: "",
      CODEX_SANDBOX: "workspace-write",
      FAKE_CODEX_LOG: rpcLog,
    }),
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  launcher.stdout.on("data", (data) => { stdout += data; });
  launcher.stderr.on("data", (data) => { stderr += data; });

  try {
    await waitFor(() => stdout.includes("\n") && stdout);
    const summary = JSON.parse(stdout.trim().split("\n")[0]);
    assert.equal(summary.workers.length, 6);
    assert.equal(summary.workers[0].model, "gpt-5.6-luna");

    const refused = runScript("codex-swarm.mjs", [], {
      status: 2,
      env: {
        CODEX_BIN: fakeCodex,
        CODEX_WAVE: "smoke",
        FAKE_CODEX_LOG: rpcLog,
      },
    });
    assert.match(refused.stderr, /launcher=.* is active/);

    writeJson(join(STATE, "codex-tasks.overlap.json"), [{
      name: "worker-overlap",
      role: "implementer",
      cwd: worktrees[3].path,
      branch: worktrees[3].branch,
      objective: "Prove the cross-wave worktree lock.",
      prompt: "Do not start.",
    }]);
    const overlap = runScript("codex-swarm.mjs", [], {
      status: 2,
      env: {
        CODEX_BIN: fakeCodex,
        CODEX_WAVE: "overlap",
        FAKE_CODEX_LOG: rpcLog,
      },
    });
    assert.match(overlap.stderr, /worktree .* is active in wave smoke/);

    const ready = JSON.parse(readFileSync(join(STATE, "codex-swarm-ready.smoke.json"), "utf8"));
    assert.equal(ready.threadIds.length, 6);
    assert.equal(ready.runId, summary.runId);

    const statusPath = join(STATE, "codex-swarm-status.smoke.json");
    const status = await waitFor(() => {
      if (!existsSync(statusPath)) return null;
      const value = JSON.parse(readFileSync(statusPath, "utf8"));
      const byName = Object.fromEntries(value.map((item) => [item.name, item]));
      return byName["worker-complete"]?.turnStatus === "completed"
        && byName["worker-limited"]?.turnStatus === "failed"
        && byName["worker-reviewer"]?.turnStatus === "interrupted"
        && byName["worker-active"]?.turnId === "turn-thread-4-2"
        && byName["worker-waiting"]?.turnStatus === "waiting"
        ? value
        : null;
    });
    const byName = Object.fromEntries(status.map((item) => [item.name, item]));
    assert.ok(status.every(row => row.orchestratorId === "fixture-orchestrator"));
    assert.ok(status.every(row => row.orchestratorName === "Fixture lead"));
    assert.equal(byName["worker-limited"].goalStatus, "budgetLimited");
    assert.match(byName["worker-limited"].error, /goal budgetLimited/);
    assert.equal(byName["worker-reviewer"].role, "reviewer");
    assert.equal(byName["worker-active"].turnStatus, "running");
    assert.equal("prompt" in byName["worker-active"], false);
    assert.match(byName["worker-active"].boardOwner, /^smoke:[0-9a-f-]+:worker-active$/);

    const calls = readRpc(rpcLog);
    const threadStarts = calls.filter((call) => call.method === "thread/start");
    const turnStarts = calls.filter((call) => call.method === "turn/start");
    assert.equal(threadStarts.length, 6);
    assert.equal("approvalPolicy" in threadStarts[0].params, false);
    assert.equal(threadStarts[0].params.sandbox, "workspace-write");
    assert.equal(threadStarts[2].params.sandbox, "read-only");
    assert.equal("effort" in threadStarts[0].params, false);
    assert.equal(turnStarts[0].params.model, "gpt-5.6-luna");
    assert.equal(turnStarts[0].params.effort, "max");
    assert.equal(turnStarts[0].params.sandboxPolicy.type, "workspaceWrite");
    assert.deepEqual(
      turnStarts[0].params.sandboxPolicy.writableRoots,
      [realpathSync(worktrees[0].path), realpathSync(join(STATE, "board"))],
    );
    assert.equal(turnStarts[0].params.sandboxPolicy.excludeSlashTmp, true);
    assert.equal(turnStarts[0].params.sandboxPolicy.excludeTmpdirEnvVar, true);
    assert.equal(turnStarts[2].params.sandboxPolicy.type, "readOnly");
    assert.match(turnStarts[0].params.clientUserMessageId, /^[0-9a-f-]+$/);
    assert.match(turnStarts[0].params.input[0].text, /Commit each finished part/);
    assert.match(turnStarts[2].params.input[0].text, /Do not edit files\. Do not commit\./);
    assert.doesNotMatch(turnStarts[2].params.input[0].text, /Run the affected tests/);
    assert.match(turnStarts[2].params.input[0].text, /Run only read-only checks/);

    runScript("luna", ["say", "--wave", "smoke", "worker-active", "Inspect", "one", "path."]);
    await waitFor(() => readRpc(rpcLog).some(
      (call) => call.method === "turn/steer" && call.params.threadId === "thread-4",
    ));
    const successSteer = readRpc(rpcLog).find(
      (call) => call.method === "turn/steer" && call.params.threadId === "thread-4",
    );
    assert.equal(successSteer.params.expectedTurnId, "turn-thread-4-2");
    assert.match(successSteer.params.clientUserMessageId, /^[0-9a-f-]+$/);
    assert.equal(successSteer.params.input[0].text, "Inspect one path.\n");
    assert.equal(
      existsSync(join(STATE, `codex-inbox.smoke.${summary.runId}`, "worker-active.json")),
      false,
    );

    runScript(
      "codex-steer",
      ["--wave", "smoke", "worker-waiting", "Start", "the", "next", "turn."],
    );
    await waitFor(() => readRpc(rpcLog).filter(
      (call) => call.method === "turn/start" && call.params.threadId === "thread-6",
    ).length >= 2);
    const waitingStarts = readRpc(rpcLog).filter(
      (call) => call.method === "turn/start" && call.params.threadId === "thread-6",
    );
    assert.equal(waitingStarts[1].params.input[0].text, "Start the next turn.\n");
    assert.match(waitingStarts[1].params.clientUserMessageId, /^[0-9a-f-]+$/);
    assert.equal(
      existsSync(join(STATE, `codex-inbox.smoke.${summary.runId}`, "worker-waiting.json")),
      false,
    );

    runScript(
      "codex-steer",
      ["--wave", "smoke", "worker-steer-error", "Keep", "this", "message."],
    );
    await waitFor(() => readRpc(rpcLog).filter(
      (call) => call.method === "turn/steer" && call.params.threadId === "thread-5",
    ).length >= 2);
    const failedSteers = readRpc(rpcLog).filter(
      (call) => call.method === "turn/steer" && call.params.threadId === "thread-5",
    );
    assert.equal(new Set(failedSteers.map((call) => call.params.clientUserMessageId)).size, 1);
    const deadLetters = join(STATE, `codex-dead-letter.smoke.${summary.runId}`);
    const deadLetter = await waitFor(() => {
      const files = existsSync(deadLetters) ? run("find", [deadLetters, "-type", "f"]).stdout.trim() : "";
      return files && files;
    });
    const deadMessage = JSON.parse(readFileSync(deadLetter, "utf8"));
    assert.equal(deadMessage.text, "Keep this message.\n");
    assert.equal(deadMessage.attempts, 2);

    const report = runScript("codex-report", ["--wave", "smoke"]);
    assert.match(report.stdout, /worker-limited/);
    assert.match(report.stdout, /budgetLimited/);

    const stopped = await runAsync(join(SCRIPTS, "codex-stop"), ["--wave", "smoke"]);
    assert.match(stopped.stdout, /CODEX STOPPED wave=smoke/);
    await waitFor(() => launcher.exitCode !== null || launcher.signalCode !== null);

    const stoppedStatus = JSON.parse(readFileSync(statusPath, "utf8"));
    const stoppedByName = Object.fromEntries(stoppedStatus.map((item) => [item.name, item]));
    assert.equal(stoppedByName["worker-active"].turnStatus, "interrupted");
    assert.equal(stoppedByName["worker-steer-error"].turnStatus, "interrupted");
    assert.equal(stoppedByName["worker-waiting"].turnStatus, "interrupted");

    const failedWatch = runScript(
      "codex-watch",
      ["--wave", "smoke", "--poll", "0.05", "--start-timeout", "1"],
      { status: 1 },
    );
    assert.match(failedWatch.stdout, /failures=5/);
    const failedWatchAgain = runScript(
      "codex-watch",
      ["--wave", "smoke", "--poll", "0.05", "--start-timeout", "1"],
      { status: 1 },
    );
    assert.match(failedWatchAgain.stdout, /failures=5/);
  } finally {
    if (launcher.exitCode === null && launcher.signalCode === null) launcher.kill("SIGTERM");
    await waitFor(() => launcher.exitCode !== null || launcher.signalCode !== null).catch(() => null);
  }

  assert.equal(stderr, "");
}

function testBoard() {
  const ownerA = "wave:run-a:worker-a";
  const ownerB = "wave:run-b:worker-b";
  runScript("codex-board", ["claim", "shared-build", ownerA, "smoke"]);
  const held = runScript("codex-board", ["claim", "shared-build", ownerB], { status: 1 });
  assert.match(held.stdout, /HELD shared-build/);
  runScript("codex-board", ["renew", "shared-build", ownerA]);
  runScript(
    "codex-board",
    ["takeover", "shared-build", ownerB, "wrong-owner"],
    { status: 1 },
  );
  runScript("codex-board", ["takeover", "shared-build", ownerB, ownerA, "verified"]);
  runScript("codex-board", ["note", ownerB, "board smoke passed"]);
  const board = runScript("codex-board", ["show"]);
  assert.match(board.stdout, /CLAIM shared-build\s+wave:run-b:worker-b/);
  assert.match(board.stdout, /NOTE\s+wave:run-b:worker-b/);
  runScript("codex-board", ["release", "shared-build", ownerB]);
}

function spawnBoard(args, pollMs = "50") {
  const child = spawn(join(SCRIPTS, "codex-board"), args, {
    env: environment({ CODEX_BOARD_POLL_MS: pollMs }),
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.capturedStdout = "";
  child.capturedStderr = "";
  child.stdout.on("data", (data) => { child.capturedStdout += data; });
  child.stderr.on("data", (data) => { child.capturedStderr += data; });
  return child;
}

async function waitQueued(resource, position, worker) {
  const pattern = new RegExp(`QUEUE ${resource}\\s+${position}\\s+${worker}`);
  await waitFor(() => {
    const show = runScript("codex-board", ["show"]);
    return pattern.test(show.stdout) ? show : null;
  });
}

async function testBoardWaitQueueOrder() {
  const resource = "wait-build";
  const ownerA = "wave:run:worker-a";
  const ownerB = "wave:run:worker-b";
  const ownerC = "wave:run:worker-c";

  runScript("codex-board", ["claim", resource, ownerA]);

  const waiterB = spawnBoard(["claim", resource, ownerB, "--wait"]);
  await waitQueued(resource, 1, ownerB);

  const waiterC = spawnBoard(["claim", resource, ownerC, "--wait"]);
  await waitQueued(resource, 2, ownerC);

  try {
    runScript("codex-board", ["release", resource, ownerA]);
    await waitFor(() => waiterB.exitCode !== null);
    assert.equal(waiterB.exitCode, 0, waiterB.capturedStderr);
    assert.match(waiterB.capturedStdout, new RegExp(`CLAIMED ${resource} ${ownerB}`));

    // C stays queued behind B: not granted early, process still running.
    await delay(200);
    assert.equal(waiterC.exitCode, null);
    const mid = runScript("codex-board", ["show"]);
    assert.match(mid.stdout, new RegExp(`CLAIM ${resource}\\s+${ownerB}`));
    assert.match(mid.stdout, new RegExp(`QUEUE ${resource}\\s+1\\s+${ownerC}`));

    runScript("codex-board", ["release", resource, ownerB]);
    await waitFor(() => waiterC.exitCode !== null);
    assert.equal(waiterC.exitCode, 0, waiterC.capturedStderr);
    assert.match(waiterC.capturedStdout, new RegExp(`CLAIMED ${resource} ${ownerC}`));

    runScript("codex-board", ["release", resource, ownerC]);
    const after = runScript("codex-board", ["show"]);
    assert.doesNotMatch(after.stdout, new RegExp(`(CLAIM|QUEUE) ${resource}`));
  } finally {
    for (const child of [waiterB, waiterC]) {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    }
  }
}

async function testBoardWaitTimeout() {
  const resource = "wait-timeout";
  const ownerD = "wave:run:worker-d";
  const ownerE = "wave:run:worker-e";

  runScript("codex-board", ["claim", resource, ownerD]);
  const timedOut = runScript(
    "codex-board",
    ["claim", resource, ownerE, "--wait", "--timeout", "0.3"],
    { status: 3, env: { CODEX_BOARD_POLL_MS: "50" }, timeout: 5000 },
  );
  assert.match(
    timedOut.stdout,
    new RegExp(`TIMEOUT ${resource} ${ownerE}: held by ${ownerD} for \\d+s, queue position 1 of 1`),
  );
  const afterTimeout = runScript("codex-board", ["show"]);
  assert.doesNotMatch(afterTimeout.stdout, /worker-e/);
  assert.match(afterTimeout.stdout, new RegExp(`CLAIM ${resource}\\s+${ownerD}`));
  runScript("codex-board", ["release", resource, ownerD]);
}

async function testBoardWaitDeadWaiter() {
  const resource = "wait-dead";
  const ownerA = "wave:run:worker-a";
  const ownerB = "wave:run:worker-b";

  runScript("codex-board", ["claim", resource, ownerA]);
  const deadWaiter = spawnBoard(["claim", resource, ownerB, "--wait"]);
  await waitQueued(resource, 1, ownerB);

  deadWaiter.kill("SIGKILL");
  await waitFor(() => deadWaiter.exitCode !== null || deadWaiter.signalCode !== null);
  // Give the OS a moment to actually reap the pid before the next operation
  // checks liveness with kill(pid, 0).
  await delay(100);

  // The next operation on the resource prunes the dead waiter; nothing
  // claims on its behalf, so the resource is simply free afterward.
  runScript("codex-board", ["release", resource, ownerA]);
  const after = runScript("codex-board", ["show"]);
  assert.doesNotMatch(after.stdout, new RegExp(`(CLAIM|QUEUE) ${resource}`));
}

async function testBoardPlainClaimDoesNotJumpQueue() {
  // A plain `claim` (no --wait) is what every existing poll loop runs. It
  // must not win a resource out from under a live --wait queue the instant
  // the holder releases: that is the same starvation --wait exists to
  // fix, just moved to the moment of release.
  const resource = "wait-priority";
  const ownerA = "wave:run:worker-a";
  const ownerB = "wave:run:worker-b";
  const ownerC = "wave:run:worker-c";

  runScript("codex-board", ["claim", resource, ownerA]);
  // Give B a long poll interval: it registers in the queue immediately
  // (below), then does not check again for a while, leaving a wide,
  // deterministic window to prove a plain claim from C is blocked while
  // the resource is free but B is still queued ahead of it.
  const waiterB = spawnBoard(["claim", resource, ownerB, "--wait"], "1500");
  await waitQueued(resource, 1, ownerB);

  runScript("codex-board", ["release", resource, ownerA]);

  const blocked = runScript("codex-board", ["claim", resource, ownerC], { status: 1 });
  assert.match(
    blocked.stdout,
    new RegExp(`QUEUED ${resource} 1 waiting, head=${ownerB}`),
  );
  // C never joined the queue and never took the resource.
  const midShow = runScript("codex-board", ["show"]);
  assert.doesNotMatch(midShow.stdout, new RegExp(`CLAIM ${resource}`));
  assert.match(midShow.stdout, new RegExp(`QUEUE ${resource}\\s+1\\s+${ownerB}`));

  try {
    await waitFor(() => waiterB.exitCode !== null, 5000);
    assert.equal(waiterB.exitCode, 0, waiterB.capturedStderr);
    assert.match(waiterB.capturedStdout, new RegExp(`CLAIMED ${resource} ${ownerB}`));
  } finally {
    if (waiterB.exitCode === null && waiterB.signalCode === null) waiterB.kill("SIGTERM");
  }

  runScript("codex-board", ["release", resource, ownerB]);
}

async function testWatchBehavior() {
  const runId = "watch-success-run";
  writeJson(join(STATE, "codex-swarm-status.watch-success.json"), [{
    wave: "watch-success",
    runId,
    launcherPid: process.pid,
    name: "worker-done",
    threadId: "thread-done",
    branch: "codex/done",
    requestedModel: "gpt-5.6-luna",
    model: "gpt-5.6-luna",
    effort: "max",
    events: 4,
    turnStatus: "completed",
  }]);
  writeJson(join(STATE, "codex-swarm-ready.watch-success.json"), {
    wave: "watch-success",
    runId,
    launcherPid: process.pid,
    readyAt: new Date().toISOString(),
    threadIds: ["thread-done"],
  });
  const watch = runScript(
    "codex-watch",
    ["--wave", "watch-success", "--poll", "0.05", "--start-timeout", "1"],
  );
  assert.match(watch.stdout, /CODEX COMPLETED worker-done/);
  assert.match(watch.stdout, /CODEX WAVE DONE 1 workers failures=0/);

  writeFileSync(join(STATE, "codex-swarm-ready.watch-corrupt.json"), "{bad json\n");
  const corrupt = runScript(
    "codex-watch",
    ["--wave", "watch-corrupt", "--poll", "0.05", "--start-timeout", "1"],
    { status: 2 },
  );
  assert.match(corrupt.stderr, /CODEX WATCH ERROR/);

  const replaceStatus = {
    wave: "watch-replace",
    runId: "replace-run-a",
    launcherPid: process.pid,
    name: "worker-replace",
    threadId: "thread-replace-a",
    branch: "codex/replace",
    requestedModel: "gpt-5.6-luna",
    model: "gpt-5.6-luna",
    effort: "max",
    events: 1,
    startedAt: new Date().toISOString(),
    turnStatus: "running",
  };
  writeJson(join(STATE, "codex-swarm-status.watch-replace.json"), [replaceStatus]);
  writeJson(join(STATE, "codex-swarm-ready.watch-replace.json"), {
    wave: "watch-replace",
    runId: "replace-run-a",
    launcherPid: process.pid,
    readyAt: new Date().toISOString(),
    threadIds: ["thread-replace-a"],
  });
  const watcher = spawn(join(SCRIPTS, "codex-watch"), [
    "--wave", "watch-replace", "--poll", "0.05", "--start-timeout", "2",
  ], {
    env: environment(),
    stdio: ["ignore", "pipe", "pipe"],
  });
  let watcherStderr = "";
  watcher.stderr.on("data", (data) => { watcherStderr += data; });
  await delay(150);
  writeJson(join(STATE, "codex-swarm-ready.watch-replace.json"), {
    wave: "watch-replace",
    runId: "replace-run-b",
    launcherPid: process.pid,
    readyAt: new Date().toISOString(),
    threadIds: ["thread-replace-b"],
  });
  const watcherStatus = await new Promise((resolveStatus) => watcher.on("exit", resolveStatus));
  assert.equal(watcherStatus, 1);
  assert.match(watcherStderr, /CODEX WAVE REPLACED/);

  writeJson(join(STATE, "codex-swarm-status.watch-stall.json"), [{
    ...replaceStatus,
    wave: "watch-stall",
    runId: "stall-run",
    name: "worker-stall",
    threadId: "thread-stall",
    startedAt: new Date(Date.now() - 5_000).toISOString(),
  }]);
  writeJson(join(STATE, "codex-swarm-ready.watch-stall.json"), {
    wave: "watch-stall",
    runId: "stall-run",
    launcherPid: process.pid,
    readyAt: new Date().toISOString(),
    threadIds: ["thread-stall"],
  });
  const stalled = runScript(
    "codex-watch",
    [
      "--wave", "watch-stall", "--poll", "0.05", "--stall-seconds", "0.1",
      "--start-timeout", "1",
    ],
    { status: 1 },
  );
  assert.match(stalled.stdout, /CODEX STALLED worker-stall/);
  assert.match(stalled.stderr, /CODEX WAVE STALLED/);
}

try {
  testBoard();
  await testBoardWaitQueueOrder();
  await testBoardWaitTimeout();
  await testBoardWaitDeadWaiter();
  await testBoardPlainClaimDoesNotJumpQueue();
  await testWatchBehavior();
  await testLauncher();
  passed = true;
  console.log("portable smoke: PASS");
} finally {
  if (passed) {
    rmSync(ROOT, { recursive: true, force: true });
  } else {
    console.error(`smoke state kept at ${ROOT}`);
  }
}
