// Launch N independent Codex threads, one per task, each in its own worktree.
// Shapes read from the v2 schema generated for the installed CLI.
import { execSync, spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { appendFileSync, writeFileSync, readFileSync } from "node:fs";

// Default to luna: 34 threads completed on it against 6 failures, and every
// one of those 6 was "Selected model is at capacity", which is now retried on
// the same thread rather than counted as a dead thread.
// null means take the server default and record what it resolved to.
const MODEL = process.env.CODEX_MODEL || "gpt-5.6-luna";
const EFFORT = "max";
// tokenBudget is enforced by the server, not advisory: a direct query returned
// status=budgetLimited used=421133 budget=400000. Keep it far above any real
// wave, or threads stop mid-task and commit nothing.
const BUDGET = Number(process.env.CODEX_BUDGET || 100_000_000);
const TMP = "/Users/igor/.claude/jobs/b02da0e1/tmp";
// Worker rules live in one file, prepended to every prompt. Hand copying them
// into each task is how they drift: the same preamble was retyped for every
// wave in one session and lost a clause each time.
const PREAMBLE = (() => {
  const path = process.env.CODEX_PREAMBLE
    || "/Users/igor/Projects/lumina/workspace-operations/skills/codex-agents/prompts/worker-preamble.md";
  try { return readFileSync(path, "utf8") + "\n\n---\n\n"; } catch { return ""; }
})();
const tasks = JSON.parse(readFileSync(`${TMP}/codex-tasks.json`, "utf8"));
const WAVE = process.env.CODEX_WAVE || "main";
const STATUS = `${TMP}/codex-swarm-status.${WAVE}.json`;

// Refuse to start on top of live threads. Killing the app-server to launch a
// wave kills every thread it hosts, including ones mid-build: that is how a
// thread with 10,940 events was lost with nothing committed. CODEX_FORCE=1
// overrides, deliberately and visibly.
// A thread is only live if a launcher is alive to drive it. "running" in the
// status file just means nobody wrote a terminal status: when the launcher is
// killed, every thread it hosts is frozen there forever, and the guard below
// then refuses every future wave on ghosts. That blocked the machine four
// times in one hour. The launcher records its own pid; liveness is kill(pid,0).
const LAUNCHER_PID_FILE = `${TMP}/codex-swarm-launcher.pid`;
const launcherAlive = (() => {
  try {
    const pid = Number(readFileSync(LAUNCHER_PID_FILE, "utf8").trim());
    if (!pid || pid === process.pid) return false;
    process.kill(pid, 0);
    return true;
  } catch { return false; }
})();

try {
  const live = launcherAlive
    ? JSON.parse(readFileSync(STATUS, "utf8")).filter((t) => t.turnStatus === "running")
    : [];
  if (live.length && process.env.CODEX_FORCE !== "1") {
    console.error(`CODEX_SWARM_REFUSED ${live.length} thread(s) still running: ` +
      live.map((t) => t.name).join(", ") + " (set CODEX_FORCE=1 to override)");
    process.exit(2);
  }
} catch { /* no previous wave */ }

writeFileSync(LAUNCHER_PID_FILE, String(process.pid));

const proc = spawn("codex", ["app-server", "--listen", "stdio://"], { stdio: ["pipe", "pipe", "pipe"] });
let nextId = 1;
const pending = new Map();
const byThread = new Map();

function call(method, params = {}) {
  const id = nextId++;
  proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

function flush() {
  writeFileSync(STATUS, JSON.stringify([...byThread.values()], null, 1));
}

createInterface({ input: proc.stdout }).on("line", (line) => {
  let msg; try { msg = JSON.parse(line); } catch { return; }
  if (msg.id !== undefined && pending.has(msg.id)) {
    const p = pending.get(msg.id); pending.delete(msg.id);
    msg.error ? p.reject(msg.error) : p.resolve(msg.result);
    return;
  }
  const tid = msg.params?.threadId ?? msg.params?.thread?.id;
  const s = tid && byThread.get(tid);
  if (s) {
    s.events++;
    s.lastEvent = new Date().toISOString();
    // The model the run actually used, not the one requested.
    if (msg.method === "item/agentMessage/delta" && msg.params?.delta) s.tail = (s.tail + msg.params.delta).slice(-300);
    // The server owns the real verdict. A thread stopped by tokenBudget keeps
    // its last turn status, so a watcher reading turn events alone shows it as
    // running forever. thread/goal/updated carries the truth.
    if (msg.method === "thread/goal/updated") {
      const status = msg.params?.goal?.status;
      s.goalStatus = status ?? s.goalStatus;
      s.tokensUsed = msg.params?.goal?.tokensUsed ?? s.tokensUsed;
      if (status === "budgetLimited" || status === "usageLimited") {
        s.turnStatus = "failed";
        s.error = `goal ${status} at ${s.tokensUsed} tokens`;
      } else if (status === "complete") {
        s.turnStatus = "completed";
      }
    }
    if (msg.method === "turn/completed") {
      const error = msg.params?.turn?.error;
      s.turnStatus = error ? "failed" : "completed";
      s.error = error?.message ?? null;
      // "Selected model is at capacity" is a server refusal of this turn, not a
      // dead thread: the thread keeps its history and can take another turn.
      // Treated as failure it cost 11,149 events of uncommitted work in one
      // thread alone. Re-issue on the same thread, with a widening backoff.
      if (/at capacity/i.test(s.error ?? "") && s.capacityRetries < CAPACITY_RETRY_MAX) {
        s.capacityRetries += 1;
        s.turnStatus = "capacity-retry";
        const wait = CAPACITY_BACKOFF_MS * s.capacityRetries;
        appendFileSync(`${TMP}/codex-swarm-events.jsonl`,
          JSON.stringify({ name: s.name, capacityRetry: s.capacityRetries, waitMs: wait }) + "\n");
        setTimeout(() => { void reissueTurn(s); }, wait);
      }
    }
    if (msg.method && !msg.method.includes("Delta")) {
      appendFileSync(`${TMP}/codex-swarm-events.jsonl`,
        JSON.stringify({ at: s.lastEvent, name: s.name, method: msg.method }) + "\n");
    }
    flush();
  }
});
const CAPACITY_RETRY_MAX = 6;
const CAPACITY_BACKOFF_MS = 120_000;

async function reissueTurn(s) {
  try {
    const turn = await call("turn/start", {
      threadId: s.threadId,
      ...(MODEL ? { model: MODEL } : {}),
      effort: EFFORT,
      approvalPolicy: "never",
      sandboxPolicy: { type: "dangerFullAccess" },
      cwd: s.cwd,
      runtimeWorkspaceRoots: [s.cwd],
      input: [{ type: "text", text: s.prompt }],
    });
    s.turnId = turn?.turn?.id ?? s.turnId;
    s.turnStatus = "running";
    s.error = null;
  } catch (error) {
    s.turnStatus = "failed";
    s.error = `capacity retry ${s.capacityRetries} failed: ${error?.message ?? error}`;
  }
  flush();
}

proc.stderr.on("data", (b) => appendFileSync(`${TMP}/codex-swarm-stderr.log`, b.toString()));

await call("initialize", {
  clientInfo: { name: "attar-orchestrator", version: "1" },
  capabilities: { experimentalApi: true },
});

for (const task of tasks) {
  const started = await call("thread/start", {
    cwd: task.cwd,
    ...(MODEL ? { model: MODEL } : {}),
    approvalPolicy: "never",
    sandbox: "danger-full-access",
    runtimeWorkspaceRoots: [task.cwd],
    threadSource: "subagent",
    developerInstructions:
      `You are an autonomous Codex coding thread. Work only in ${task.cwd}. ` +
      `Commit completed changes on ${task.branch}. Do not modify other worktrees or the main checkout. Do not push. ` +
      `This machine is shared with other threads: run uptime before any heavy step and wait if load average is above 20.`,
  });
  const threadId = started?.thread?.id;
  byThread.set(threadId, {
    name: task.name, threadId, turnId: null, branch: task.branch, cwd: task.cwd,
    requestedModel: MODEL, model: started?.model ?? null, error: null, effort: EFFORT,
    events: 0, lastEvent: null, turnStatus: "running", goalStatus: null, tokensUsed: 0, tail: "",
    prompt: PREAMBLE + task.prompt, capacityRetries: 0,
  });

  try {
    await call("thread/goal/set", { threadId, objective: task.objective, tokenBudget: BUDGET, status: "active" });
  } catch (error) {
    appendFileSync(`${TMP}/codex-swarm-events.jsonl`, JSON.stringify({ name: task.name, goalError: error }) + "\n");
  }

  const turn = await call("turn/start", {
    threadId,
    ...(MODEL ? { model: MODEL } : {}),
    effort: EFFORT,
    approvalPolicy: "never",
    sandboxPolicy: { type: "dangerFullAccess" },
    cwd: task.cwd,
    runtimeWorkspaceRoots: [task.cwd],
    input: [{ type: "text", text: PREAMBLE + task.prompt }],
  });
  byThread.get(threadId).turnId = turn?.turn?.id ?? null;
  flush();
}

// Steering inbox. A thread lives inside the app-server process that created it,
// and a second process gets "already has an active writer" from thread/resume.
// So the launcher stays alive and forwards messages dropped as files. Write
// codex-inbox/<name>.txt and it becomes a follow-up turn on that thread.
import { existsSync, mkdirSync, readdirSync, unlinkSync } from "node:fs";
const INBOX = `${TMP}/codex-inbox`;
mkdirSync(INBOX, { recursive: true });
setInterval(async () => {
  for (const file of readdirSync(INBOX)) {
    if (!file.endsWith(".txt")) continue;
    const name = file.slice(0, -4);
    const entry = [...byThread.values()].find((t) => t.name === name);
    if (!entry) continue;
    const path = `${INBOX}/${file}`;
    const text = readFileSync(path, "utf8");
    unlinkSync(path);
    try {
      await call("turn/start", {
        threadId: entry.threadId, model: MODEL || undefined, effort: EFFORT,
        approvalPolicy: "never", sandboxPolicy: { type: "dangerFullAccess" },
        cwd: entry.cwd, runtimeWorkspaceRoots: [entry.cwd],
        input: [{ type: "text", text }],
      });
      entry.turnStatus = "running";
      appendFileSync(`${TMP}/codex-swarm-events.jsonl`,
        JSON.stringify({ steered: name, chars: text.length }) + "\n");
      flush();
    } catch (error) {
      appendFileSync(`${TMP}/codex-swarm-events.jsonl`,
        JSON.stringify({ steerError: name, error }) + "\n");
    }
  }
}, 10000);

console.log(JSON.stringify([...byThread.values()].map((s) => ({ name: s.name, threadId: s.threadId, model: s.model }))));
