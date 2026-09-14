#!/usr/bin/env node
// Isolated fake native protocol. No installed Codex or model service is used.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import { runInNewContext } from "node:vm";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const root = mkdtempSync(join(tmpdir(), "swarm-retry-contract-"));
const source = join(root, "source"), worker = join(root, "worker");
function git(...args) {
  const result = spawnSync("git", args, { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
}
git("init", "-b", "main", source);
git("-C", source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Fixture");
git("-C", source, "worktree", "add", "-b", "fixture", worker);
const fake = join(root, "fake-native.mjs");
writeFileSync(fake, `#!/usr/bin/env node
import { createInterface } from "node:readline";
import { appendFileSync, readFileSync, readdirSync } from "node:fs";
const mode = process.env.FIXTURE_MODE;
let starts = 0, steers = 0;
const send = (value) => process.stdout.write(JSON.stringify(value) + "\\n");
const event = (method, params) => send({ method, params: { threadId: "thread-1", ...params } });
const started = (id) => event("turn/started", { turn: { id, status: "inProgress" } });
const complete = (id, capacity = false) => event("turn/completed", { turn: { id, status: capacity ? "failed" : "completed", error: capacity ? { message: "Model is at capacity", codexErrorInfo: "serverOverloaded" } : null } });
const goal = (status) => event("thread/goal/updated", { goal: { status } });
createInterface({ input: process.stdin }).on("line", (line) => {
 const q = JSON.parse(line), p = q.params;
 appendFileSync(process.env.FIXTURE_LOG, line + "\\n");
 if (["turn/start", "turn/steer"].includes(q.method)) {
  const directory = process.env.CODEX_AGENTS_STATE_DIR + "/codex-submissions";
  const receipt = readdirSync(directory).filter((file) => file.endsWith(".json")).map((file) => JSON.parse(readFileSync(directory + "/" + file, "utf8"))).find((value) => value.requestId === q.id);
  if (!receipt || receipt.status !== "submitting") throw new Error("Native input arrived without a durable submission receipt");
 }
 const reply = (result) => send({ id: q.id, result });
 if (q.method === "thread/start") return reply({ thread: { id: "thread-1" }, model: p.model });
 if (q.method === "turn/start") {
  const id = "turn-" + (++starts);
  if (starts === 1 && ["initial_late", "initial_late_no_event", "capacity_late_receipt"].includes(mode)) {
   if (mode !== "initial_late_no_event") started(id);
   if (mode === "capacity_late_receipt") complete(id, true);
   return setTimeout(() => reply({ turn: { id } }), 850);
  }
  if (starts > 1 && mode === "start_late") {
   started(id); complete(id);
   started("newer-turn");
   return setTimeout(() => reply({ turn: { id } }), 850);
  }
  reply({ turn: { id } });
  if (starts === 1 && mode.startsWith("capacity")) {
   complete(id, true);
   if (mode === "capacity_stale") setTimeout(() => { started("newer-turn"); complete("newer-turn"); goal("complete"); }, 20);
   if (mode === "capacity_running") setTimeout(() => { started("newer-turn"); complete(id, true); }, 20);
   if (mode === "capacity_duplicate") setTimeout(() => complete(id, true), 20);
   if (mode === "capacity_goal") setTimeout(() => goal("complete"), 20);
  }
  if (mode === "start_late" || mode === "new_task") {
   complete(id);
   if (starts === 1 && mode === "new_task") goal("complete");
  }
  return;
 }
 if (q.method === "turn/steer") {
  steers += 1;
  if (mode === "steer_missing") return;
  if (mode === "steer_unknown") return send({ id: q.id, error: { code: -32000, message: "unknown result" } });
  if (mode === "steer_wrong") return reply({ turnId: "wrong-turn" });
  if (mode === "steer_reject" && steers === 1) return send({ id: q.id, error: { code: -32602, message: "rejected before native apply" } });
  if (mode === "steer_late_reject" && steers === 1) return setTimeout(() => send({ id: q.id, error: { code: -32602, message: "rejected before native apply" } }), 850);
  if (mode === "steer_late") {
   complete(p.expectedTurnId); started("newer-turn");
   return setTimeout(() => reply({ turnId: p.expectedTurnId }), 850);
  }
  return reply({ turnId: p.expectedTurnId });
 }
 reply({});
});
`, { mode: 0o755 });
const json = (path) => JSON.parse(readFileSync(path, "utf8"));
async function until(fn, description) {
  const deadline = Date.now() + 6500;
  while (Date.now() < deadline) {
    const value = fn();
    if (value) return value;
    await delay(15);
  }
  throw new Error(`Timed out: ${description}`);
}
async function launch(mode) {
  const state = join(root, mode); mkdirSync(state);
  const log = join(state, "rpc.jsonl"), tasks = join(state, "tasks.json");
  writeFileSync(tasks, JSON.stringify([{ name: "worker", cwd: worker, branch: "fixture", objective: "Fixture", prompt: "Initial fixture input" }]));
  const env = { ...process.env, CODEX_BIN: fake, CODEX_HOME: join(root, "empty-profile"), CODEX_AGENTS_STATE_DIR: state,
    CODEX_TASKS: tasks, CODEX_WAVE: "fixture", CODEX_RPC_TIMEOUT_MS: "400", CODEX_INBOX_POLL_MS: "20", CODEX_INBOX_BACKOFF_MS: "20",
    CODEX_INBOX_RETRIES: "3", CODEX_CAPACITY_BACKOFF_MS: "300", CODEX_CAPACITY_RETRIES: "2", FIXTURE_MODE: mode, FIXTURE_LOG: log };
  delete env.OPENAI_API_KEY; delete env.CODEX_API_KEY;
  const proc = spawn(process.execPath, [join(repo, "scripts/codex-swarm.mjs")], { env, stdio: ["ignore", "pipe", "pipe"] });
  let output = ""; proc.stdout.on("data", (data) => output += data); proc.stderr.on("data", (data) => output += data);
  const readyPath = join(state, "codex-swarm-ready.fixture.json");
  await until(() => { assert.equal(proc.exitCode, null, output); return existsSync(readyPath); }, "launcher ready");
  const ready = json(readyPath), inbox = join(state, `codex-inbox.fixture.${ready.runId}`, "worker.json");
  const calls = () => readFileSync(log, "utf8").trim().split("\n").map(JSON.parse);
  const mutations = () => calls().filter((q) => ["turn/start", "turn/steer"].includes(q.method));
  const receipt = (id = "message-1") => readdirSync(join(state, "codex-submissions")).filter((file) => file.endsWith(".json"))
    .map((file) => json(join(state, "codex-submissions", file))).find((value) => value.messageId === id);
  const send = (id = "message-1", text = "One user instruction") => writeFileSync(inbox, JSON.stringify({ id, text, attempts: 0, nextAttemptAt: 0 }));
  const status = () => json(join(state, "codex-swarm-status.fixture.json"))[0];
  const stop = async () => { proc.kill("SIGTERM"); await until(() => proc.exitCode !== null || proc.signalCode !== null, "fixture stopped"); };
  return { env, proc, state, inbox, readyPath, calls, mutations, receipt, send, status, stop };
}
let count = 0;
async function test(mode, check) {
  const h = await launch(mode);
  try { await check(h); count++; console.log(`PASS ${mode}`); }
  finally { await h.stop(); }
}
try {
  const launcherSource = readFileSync(join(repo, "scripts/codex-swarm.mjs"), "utf8");
  const callSource = launcherSource.slice(launcherSource.indexOf("function submissionError("), launcherSource.indexOf("function receiptPath("));
  const context = { nextId: 1, pending: new Map(), RPC_TIMEOUT_MS: 30, shuttingDown: false, setTimeout, clearTimeout,
    proc: { stdin: { destroyed: true, writable: false, write() { assert.fail("Closed input must not be written"); } } } };
  await assert.rejects(runInNewContext(callSource + '\ncall("turn/steer")', context), (error) => error.notSubmitted === true);
  assert.equal(context.pending.size, 0);
  count++; console.log("PASS confirmed local rejection before write");
  for (const mode of ["steer_late", "start_late"]) await test(mode, async (h) => {
    h.send();
    await until(() => existsSync(h.inbox) && json(h.inbox).delivery === "uncertain", "uncertain mailbox");
    assert.equal(h.receipt().status, "uncertain");
    assert.equal(h.mutations().length, 2);
    await until(() => !existsSync(h.inbox), "exact late receipt accepted");
    assert.equal(h.receipt().status, "accepted");
    assert.equal(h.status().turnId, "newer-turn");
    assert.equal(h.mutations().length, 2);
    h.send();
    await until(() => !existsSync(h.inbox), "same message returns saved receipt");
    assert.equal(h.mutations().length, 2);
    h.send("message-1", "Different content");
    await until(() => json(h.inbox).delivery === "uncertain", "changed message rejected");
    await delay(60);
    assert.equal(h.proc.exitCode, null);
    assert.equal(h.mutations().length, 2);
  });
  for (const mode of ["steer_reject", "steer_late_reject"]) await test(mode, async (h) => {
    h.send();
    await until(() => !existsSync(h.inbox), "confirmed rejection retried");
    const calls = h.mutations().filter((q) => q.method === "turn/steer");
    assert.equal(calls.length, 2);
    assert.equal(calls[0].params.clientUserMessageId, calls[1].params.clientUserMessageId);
    assert.equal(h.receipt().status, "accepted");
    assert.equal(h.receipt().previousAttempts[0].status, "notSubmitted");
  });
  for (const mode of ["steer_missing", "steer_unknown", "steer_wrong"]) await test(mode, async (h) => {
    h.send();
    await until(() => json(h.inbox).delivery === "uncertain", "unknown submission held");
    await delay(550);
    assert.equal(h.mutations().length, 2);
    assert.equal(h.receipt().status, "uncertain");
    await h.stop();
    const status = readFileSync(join(h.state, "codex-swarm-status.fixture.json"), "utf8");
    const before = h.calls().length;
    const restart = spawnSync(process.execPath, [join(repo, "scripts/codex-swarm.mjs")], { env: h.env, encoding: "utf8", timeout: 4000 });
    assert.notEqual(restart.status, 0);
    assert.match(restart.stderr, /Unresolved native submission/);
    assert.equal(h.calls().length, before);
    assert.equal(readFileSync(join(h.state, "codex-swarm-status.fixture.json"), "utf8"), status);
    assert.equal(json(h.inbox).id, "message-1");
  });
  for (const mode of ["initial_late", "initial_late_no_event"]) await test(mode, async (h) => {
    assert.ok(h.status().pendingSubmission);
    await until(() => !h.status().pendingSubmission, "initial late receipt");
    assert.equal(h.mutations().length, 1);
    assert.equal(h.status().turnId, "turn-1");
  });
  for (const mode of ["capacity_normal", "capacity_duplicate", "capacity_late_receipt"]) await test(mode, async (h) => {
    await until(() => h.mutations().length === 2, "capacity continuation");
    const [first, retry] = h.mutations();
    assert.equal(retry.params.threadId, first.params.threadId);
    assert.deepEqual(retry.params.input, []);
    assert.equal(retry.params.clientUserMessageId, undefined);
    await delay(400);
    assert.equal(h.mutations().length, 2);
  });
  for (const mode of ["capacity_stale", "capacity_running", "capacity_goal"]) await test(mode, async (h) => {
    await delay(550);
    assert.equal(h.mutations().length, 1);
    assert.equal(h.status().capacityRetry, null);
    if (mode === "capacity_running") assert.equal(h.status().turnId, "newer-turn");
  });
  await test("capacity_new_task", async (h) => {
    h.send();
    await until(() => !existsSync(h.inbox), "new user task delivered");
    await delay(550);
    assert.equal(h.mutations().length, 2);
    assert.equal(h.mutations()[1].params.input[0].text, "One user instruction");
    assert.equal(h.status().capacityRetry, null);
  });
  await test("new_task", async (h) => {
    await until(() => h.status().goalStatus === "complete", "completed goal notification");
    h.send();
    await until(() => !existsSync(h.inbox), "new task reactivates goal");
    assert.equal(h.mutations().length, 2);
    assert.equal(h.status().goalStatus, "active");
    assert.equal(h.calls().filter((q) => q.method === "thread/goal/set").length, 2);
  });
  console.log(`${count} swarm retry contract cases passed`);
} finally { rmSync(root, { recursive: true, force: true }); }
