#!/usr/bin/env node
// Browser fixture: the real `studio-runtime-server` on a disposable state
// directory with the scripted Node provider. Seeding goes through the public
// HTTP API. Usage: node rust-ui-fixture.mjs <root> [--workers N]
// <root> is a scratch directory for the test's files; the server
// state lives in `fixtureStateDirectory(<root>)`, a sibling that must start empty.
//
// Prints the HTTP port on stdout once seeded, then serves until the process
// receives a signal (the Playwright fixture scope kills the whole group). Stdin
// JSON lines are test hooks; a hook with an "id" is answered with a JSON line
// {"id", "ok": true, ...result} on stdout:
//   {"method":"fixture/create-worker","parent":ID,"params":{...}}  POST /api/agents
//   {"id","method":"fixture/provider","agent":ID,"steps":[...]}    run provider
//       steps (see fake-codex-app-server.mjs) on the agent's turn in flight
//   {"method":"item/started","params":{"threadId",...}}   any other method is
//       a provider notification, delivered to the turn in flight on that thread
// Environment:
//   CODEX_STUDIO_RUNTIME_SERVER   server binary (default: newest cargo build)
//   CODEX_STUDIO_WEB_DIST         renderer build served by the server (default: web/dist)
//   EXECUTION_SETTINGS_CATALOG    JSON array served by the provider's model/list
//   FIXTURE_WORKER_COUNT          workers seeded under the lead (--workers wins)
//   FIXTURE_HOLD                  comma list of lead, workers, other: their
//                                 seeded turns stay in flight
//   FIXTURE_SEED                  comma list of named seeds, see SEEDS
//   FIXTURE_AGENT_STEPS           JSON object: seeded agent name -> provider
//                                 steps its first turn runs
//   FIXTURE_SCRIPT                JSON array of provider scripts (prompt, steps)
import { spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  mkdtempSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { connect, createServer } from "node:net";
import { createInterface } from "node:readline";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  ACCOUNT_RUNNING_TURNS_MAX,
  ACCOUNT_START_BURST_MAX,
} from "../../src/generated/sharedValues.ts";

const here = dirname(fileURLToPath(import.meta.url));
const repository = resolve(here, "../../../../../..");
const dist = join(repository, "workspaces/client/apps/web/dist");
const fakeCodex = join(here, "fake-codex-app-server.mjs");
/** Longest wait for the server to answer its first request, milliseconds. */
const SERVER_START_TIMEOUT_MS = 60_000;
/** Longest wait for the provider model catalog during seeding, milliseconds. */
const CATALOG_WAIT_MS = 30_000;
/** Delay between seeding retries while the catalog loads, milliseconds. */
const CATALOG_POLL_MS = 100;
/** Workers the server lets a new lead run at once, a count. */
const LEAD_DEFAULT_CONCURRENCY = 32;
/** Model of seeded workers when a custom catalog offers it. */
const WORKER_CATALOG_MODEL = "gpt-5.6-luna";
/** Prompts of the seeded agents; provider scripts select turns by prompt. */
const LEAD_PROMPT = "Review the release";
const WORKER_PROMPT = "Review one component";
const OTHER_PROMPT = "Separate task";
/** Chat messages the `messages` seed sends before its last three, a count. */
const EARLIER_MESSAGES = 105;

/** Id of the item whose questions become the lead's pending async question. */
const QUESTION_ITEM = "async-question";

/**
 * Named seeds: provider steps for seeded agents, by agent name. Each runs
 * through the same tool calls an agent makes. `lastWorker` is the name of the
 * last seeded worker.
 */
const SEEDS = {
  // A long lead report with a table, wide enough to fill the text column.
  report: () => ({
    "Release lead": [
      {
        say: [
          "I assigned the review to the team. Seven workers are active and 15 have finished.",
          "Worker 07 found a failed check. I will collect the remaining results before I prepare the release report.",
          "| Area | Result | Next step |\n| :--- | :--- | :--- |\n| Message delivery | Passed | Review retry evidence |\n| Context and limits | Passed | Check account reset time |\n| Mobile dialogs | Needs a fix | Worker 07 owns the change |",
          "**Evidence:** all reports remain available. " +
            "Each report lists the commands that were run and their output. ".repeat(
              4,
            ),
        ].join("\n\n"),
      },
    ],
  }),
  // A pending question from the lead: the lead's message carries questions.
  question: () => ({
    "Release lead": [
      {
        say: "Which scope should I review?",
        item: QUESTION_ITEM,
        extra: {
          questions: [
            {
              title: "Which scope?",
              options: ["One file", "All files"],
            },
          ],
        },
      },
    ],
  }),
  // A complaint from the lead to the user, one from a worker to the lead, and
  // a private and a broadcast chat message from that worker.
  messages: (lastWorker) => ({
    "Release lead": [
      {
        tool: "orchestration_complaint",
        arguments: {
          action: "submit",
          text: "Please confirm the release scope.",
        },
      },
    ],
    [lastWorker]: [
      // Enough history that the chat feed pages older messages.
      ...Array.from({ length: EARLIER_MESSAGES }, (_, index) => ({
        tool: "orchestration_message",
        arguments: { target: "parent", text: `Earlier finding ${index}` },
      })),
      {
        tool: "orchestration_message",
        arguments: {
          target: "parent",
          text: "A private update before the final answer.",
        },
      },
      {
        tool: "orchestration_message",
        arguments: {
          target: "broadcast",
          text: "Release checks are ready for review.",
        },
      },
      {
        tool: "orchestration_complaint",
        arguments: {
          action: "submit",
          text: "Please provide the review account.",
        },
      },
    ],
  }),
};
/** FIXTURE_HOLD names and the prompts whose turns they hold. */
const HOLD_PROMPTS = {
  lead: LEAD_PROMPT,
  workers: WORKER_PROMPT,
  other: OTHER_PROMPT,
};
/** Worker agents seeded under the lead when --workers is absent. */
const DEFAULT_WORKER_COUNT = 6;

/** Server state directory of a fixture rooted at `root`. */
export function fixtureStateDirectory(root) {
  // Beside the root, not inside it: the server's writes must not change the
  // project directory that agents work in.
  return `${root.replace(/\/+$/, "")}-fixture-state`;
}

export function serverBinary() {
  if (process.env.CODEX_STUDIO_RUNTIME_SERVER)
    return process.env.CODEX_STUDIO_RUNTIME_SERVER;
  const target = process.env.CARGO_TARGET_DIR || join(repository, "target");
  const candidates = ["release", "debug"]
    .map((profile) => join(target, profile, "studio-runtime-server"))
    .filter(existsSync);
  if (!candidates.length)
    throw new Error(
      `studio-runtime-server is not built under ${target}; run cargo build -p studio-runtime-server`,
    );
  return candidates.sort(
    (a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs,
  )[0];
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = /** @type {import("node:net").AddressInfo} */ (
        probe.address()
      );
      probe.close(() => resolvePort(port));
    });
  });
}

/**
 * @param {string} stateDir
 * @param {{ modelsFile?: string, scriptFile?: string, controlDirectory?: string }} [options]
 */
export async function startRustBackend(
  stateDir,
  { modelsFile, scriptFile, controlDirectory } = {},
) {
  const port = await freePort();
  const origin = `http://127.0.0.1:${port}`;
  // The server approves any `codex` on PATH ahead of CODEX_BIN; hide the host's.
  // That folder can also hold `node` (Homebrew puts both in /opt/homebrew/bin),
  // and the scripted provider starts through `env node`, so a private folder
  // that links only this Node comes first.
  const nodeBin = mkdtempSync(join(tmpdir(), "studio-fixture-node-"));
  symlinkSync(process.execPath, join(nodeBin, "node"));
  const path = [
    nodeBin,
    ...(process.env.PATH ?? "")
      .split(delimiter)
      .filter((entry) => !existsSync(join(entry, "codex"))),
  ].join(delimiter);
  const child = spawn(serverBinary(), ["--port", String(port)], {
    stdio: ["ignore", 2, 2],
    env: {
      ...process.env,
      PATH: path,
      CODEX_AGENTS_STATE_DIR: stateDir,
      CODEX_BIN: fakeCodex,
      ...(scriptFile ? { FAKE_CODEX_SCRIPT: scriptFile } : {}),
      ...(controlDirectory ? { FAKE_CODEX_CONTROL: controlDirectory } : {}),
      CODEX_STUDIO_WEB_DIST: process.env.CODEX_STUDIO_WEB_DIST ?? dist,
      ...(modelsFile ? { FAKE_CODEX_MODELS: modelsFile } : {}),
    },
  });
  const removeNodeBin = () => rmSync(nodeBin, { recursive: true, force: true });
  child.once("exit", removeNodeBin);
  child.once("error", removeNodeBin);
  const exited = new Promise((_, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) =>
      reject(new Error(`studio-runtime-server exited ${code ?? signal}`)),
    );
  });
  exited.catch(() => {});
  const deadline = Date.now() + SERVER_START_TIMEOUT_MS;
  for (;;) {
    if (child.exitCode !== null || child.signalCode !== null) await exited;
    try {
      if ((await fetch(`${origin}/api/session`)).ok) break;
    } catch {}
    if (Date.now() > deadline) throw new Error("server start timed out");
    await Promise.race([
      exited,
      new Promise((resolveWait) => setTimeout(resolveWait, 50)),
    ]);
  }
  const { token } = await (await fetch(`${origin}/api/session`)).json();
  const post = async (path, body) => {
    // The provider catalog loads in the background; admission reports a transient error until it lands.
    const retryUntil = Date.now() + CATALOG_WAIT_MS;
    for (;;) {
      const result = await postOnce(path, body);
      if (
        result.ok ||
        !/catalog is (unavailable|pending)/i.test(result.text) ||
        Date.now() > retryUntil
      )
        return result.ok ? JSON.parse(result.text) : fail(path, result);
      await new Promise((resolveWait) =>
        setTimeout(resolveWait, CATALOG_POLL_MS),
      );
    }
  };
  const fail = (path, result) => {
    throw new Error(`POST ${path}: ${result.status} ${result.text}`);
  };
  const postOnce = async (path, body) => {
    const response = await fetch(origin + path, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Origin: origin,
        "X-Canvas-Token": token,
      },
      body: JSON.stringify(body),
    });
    return {
      ok: response.ok,
      status: response.status,
      text: await response.text(),
    };
  };
  return { child, port, origin, token, post };
}

/** Delay between idle polls while seeded turns finish, milliseconds. */
const IDLE_POLL_MS = 100;
/** Longest wait for the seeded turns to finish, milliseconds. */
const IDLE_WAIT_MS = 60_000;
const workerName = (index) => `Worker ${String(index).padStart(2, "0")}`;
/** Longest wait for a provider control reply, milliseconds. */
const CONTROL_REPLY_MS = 10_000;

/**
 * Event kinds whose delivery the fixture waits for. Chat messages are also
 * events, but an agent drains them over several wake-ups; waiting for them
 * would take minutes. A seed that sends many messages also leaves the lead's
 * result events to that slow drain, so it waits for user events only.
 */
const WAKING_EVENT_KINDS = ["user", "child_result"];
const WAKING_EVENT_KINDS_WITH_CHATTER = ["user"];

/** Entity page size, the sync API's limit. */
const ENTITY_PAGE_SIZE = 500;

/** Entities of one collection in the snapshot the renderer also reads. */
async function readEntities(origin, collection) {
  const values = [];
  let after = 0;
  for (;;) {
    const page = await (
      await fetch(
        `${origin}/api/sync/pull?scope=state:entities:v1&after=${after}&limit=${ENTITY_PAGE_SIZE}&fresh=1`,
      )
    ).json();
    for (const document of page.documents) {
      const entity = JSON.parse(document.payload);
      if (entity.collection === collection && !document._deleted)
        values.push(entity.value);
    }
    const next = page.checkpoint.seq;
    if (next <= after || next >= (page.maxSeq ?? next)) return values;
    after = next;
  }
}

const readAgents = (origin) => readEntities(origin, "agent");

/**
 * Wait until the seeded agents are quiet: no agent that is not held has a turn
 * in flight, is queued, or has a waking event still pending. Workers report
 * their results to the lead, whose follow-up turns must finish first.
 */
async function waitUntilIdle(origin, isHeld, kinds) {
  const deadline = Date.now() + IDLE_WAIT_MS;
  for (;;) {
    const agents = await readAgents(origin);
    const held = new Set(
      agents.filter((agent) => isHeld(agent)).map((agent) => agent.id),
    );
    const busy = agents.filter(
      (agent) =>
        !held.has(agent.id) && (agent.inFlight || agent.status === "queued"),
    );
    const pending = (await readEntities(origin, "event")).filter(
      (event) =>
        event.status === "pending" &&
        kinds.includes(event.kind) &&
        !held.has(event.agent),
    );
    if (agents.length && !busy.length && !pending.length) return;
    if (Date.now() > deadline)
      throw new Error(
        `Seeded turns never finished: ${busy.map((a) => a.name)} ${pending.length} pending events`,
      );
    await new Promise((resolveWait) => setTimeout(resolveWait, IDLE_POLL_MS));
  }
}

/** Held seeded agents: FIXTURE_HOLD names to a predicate over agent records. */
const HOLD_MATCHERS = {
  lead: (agent) => agent.name === "Release lead",
  workers: (agent) => agent.name?.startsWith("Worker"),
  other: (agent) => agent.name === "Other project",
};

/**
 * Send provider steps to the process that owns `thread`. Every provider
 * process listens on its own socket in `directory`; resolves true when one of
 * them ran the steps on a turn in flight.
 */
async function sendControl(directory, thread, steps) {
  const attempts = readdirSync(directory)
    .filter((name) => name.endsWith(".sock"))
    .map(
      (name) =>
        new Promise((resolveReply) => {
          const socket = connect(join(directory, name));
          const timer = setTimeout(() => socket.destroy(), CONTROL_REPLY_MS);
          socket.once("error", () => resolveReply(false));
          socket.once("close", () => {
            clearTimeout(timer);
            resolveReply(false);
          });
          const lines = createInterface({ input: socket });
          lines.on("error", () => resolveReply(false));
          lines.once("line", (line) => {
            resolveReply(JSON.parse(line).ok);
            socket.destroy();
          });
          socket.write(JSON.stringify({ id: "control", thread, steps }) + "\n");
        }),
    );
  return (await Promise.all(attempts)).some(Boolean);
}

async function main() {
  const [root, ...flags] = process.argv.slice(2);
  const stateDir = fixtureStateDirectory(root);
  mkdirSync(stateDir, { recursive: true });
  const workersAt = flags.indexOf("--workers");
  const workers =
    workersAt >= 0
      ? Number(flags[workersAt + 1])
      : Number(process.env.FIXTURE_WORKER_COUNT ?? DEFAULT_WORKER_COUNT);
  const agentSteps = {};
  for (const seed of (process.env.FIXTURE_SEED ?? "")
    .split(",")
    .filter(Boolean)) {
    if (!(seed in SEEDS)) throw new Error(`Unknown FIXTURE_SEED entry ${seed}`);
    for (const [name, steps] of Object.entries(
      SEEDS[seed](workerName(workers - 1)),
    ))
      agentSteps[name] = [...(agentSteps[name] ?? []), ...steps];
  }
  Object.assign(
    agentSteps,
    JSON.parse(process.env.FIXTURE_AGENT_STEPS ?? "{}"),
  );
  // A scripted agent gets a prompt of its own, which selects its script.
  const promptOf = (name, prompt) =>
    name in agentSteps ? `${prompt} (${name})` : prompt;
  const held = (process.env.FIXTURE_HOLD ?? "").split(",").filter(Boolean);
  for (const name of held)
    if (!(name in HOLD_PROMPTS))
      throw new Error(`Unknown FIXTURE_HOLD entry ${name}`);
  // The state directory must stay free of foreign files.
  const scratch = mkdtempSync(join(tmpdir(), "rust-ui-fixture-"));
  process.once("exit", () => rmSync(scratch, { recursive: true, force: true }));
  let modelsFile;
  if (process.env.EXECUTION_SETTINGS_CATALOG) {
    modelsFile = join(scratch, "fake-models.json");
    writeFileSync(modelsFile, process.env.EXECUTION_SETTINGS_CATALOG);
  }
  // A custom catalog may lack the reviewer role's preferred model.
  const catalog = JSON.parse(process.env.EXECUTION_SETTINGS_CATALOG ?? "[]");
  const workerModel = catalog.find(
    (row) => row.model === WORKER_CATALOG_MODEL,
  )?.model;
  const scriptFile = join(scratch, "fake-script.json");
  writeFileSync(
    scriptFile,
    JSON.stringify([
      ...Object.entries(agentSteps).map(([name, steps]) => ({
        prompt: promptOf(
          name,
          name === "Release lead"
            ? LEAD_PROMPT
            : name === "Other project"
              ? OTHER_PROMPT
              : WORKER_PROMPT,
        ),
        // A held agent keeps its turn open after its own steps.
        steps: held.some((group) => HOLD_MATCHERS[group]({ name }))
          ? [...steps, { hold: true }]
          : steps,
      })),
      ...JSON.parse(process.env.FIXTURE_SCRIPT ?? "[]"),
      ...held.map((name) => ({
        prompt: HOLD_PROMPTS[name],
        steps: [{ hold: true }],
      })),
    ]),
  );
  const controlDirectory = join(scratch, "control");
  mkdirSync(controlDirectory);
  const backend = await startRustBackend(stateDir, {
    modelsFile,
    scriptFile,
    controlDirectory,
  });
  const stop = () => {
    backend.child.kill("SIGTERM");
  };
  process.once("SIGTERM", stop);
  process.once("SIGINT", stop);
  backend.child.once("exit", (code, signal) => {
    process.stderr.write(
      `studio-runtime-server exited code=${code} signal=${signal}\n`,
      () => process.exit(code ?? 1),
    );
  });
  // The default start throttle spaces turn starts 30 seconds apart after a
  // burst of 16 and cap running turns; seeded agents and wake-ups would wait.
  await backend.post("/api/accounts/throttle", {
    account_key: "default",
    burst: ACCOUNT_START_BURST_MAX,
    pauseSeconds: 0,
    maxRunningTurns: ACCOUNT_RUNNING_TURNS_MAX,
  });
  const cwd = repository;
  const lead = await backend.post("/api/agents", {
    name: "Release lead",
    prompt: promptOf("Release lead", LEAD_PROMPT),
    cwd,
  });
  const leadId = lead.id ?? lead.agent?.id;
  if (!leadId)
    throw new Error(`Lead creation returned ${JSON.stringify(lead)}`);
  if (workers > LEAD_DEFAULT_CONCURRENCY)
    // Workers over the lead's limit would wait for its slow retry tick.
    await backend.post("/api/conversation", {
      id: leadId,
      subagent_concurrency: workers,
      expected_mode_revision: 0,
      request_id: "fixture-concurrency",
    });
  await Promise.all([
    ...Array.from({ length: workers }, (_, index) =>
      backend.post("/api/agents", {
        name: workerName(index),
        prompt: promptOf(workerName(index), WORKER_PROMPT),
        role: "reviewer",
        ...(workerModel ? { model: workerModel } : {}),
        parent: leadId,
        cwd,
      }),
    ),
    // A second, separate root chat for specs that need an unrelated project.
    backend.post("/api/agents", {
      name: "Other project",
      prompt: promptOf("Other project", OTHER_PROMPT),
      cwd,
    }),
  ]);
  const chatter = Object.keys(agentSteps).length > 0;
  const kinds = chatter ? WAKING_EVENT_KINDS_WITH_CHATTER : WAKING_EVENT_KINDS;
  const isHeld = (agent) => held.some((name) => HOLD_MATCHERS[name](agent));
  await waitUntilIdle(backend.origin, isHeld, kinds);
  const hooks = {
    "fixture/create-worker": async (message) => {
      await backend.post("/api/agents", {
        cwd,
        ...message.params,
        parent: message.parent,
      });
    },
    "fixture/provider": async (message) => {
      const agent = (await readAgents(backend.origin)).find(
        (candidate) => candidate.id === message.agent,
      );
      if (!agent?.threadId)
        throw new Error(`Agent ${message.agent} has no provider thread`);
      if (!(await sendControl(controlDirectory, agent.threadId, message.steps)))
        throw new Error(`Agent ${agent.name} has no turn in flight`);
    },
  };
  createInterface({ input: process.stdin }).on("line", async (line) => {
    const message = JSON.parse(line);
    const hook = message.method.startsWith("fixture/")
      ? hooks[message.method]
      : async () => {
          const { threadId } = message.params;
          const steps = [
            { notify: { method: message.method, params: message.params } },
          ];
          if (!(await sendControl(controlDirectory, threadId, steps)))
            throw new Error(`Thread ${threadId} has no turn in flight`);
        };
    if (!hook) throw new Error(`Unknown fixture hook ${message.method}`);
    try {
      const result = await hook(message);
      if (message.id !== undefined)
        process.stdout.write(
          JSON.stringify({ id: message.id, ok: true, ...result }) + "\n",
        );
    } catch (error) {
      if (message.id === undefined) throw error;
      process.stdout.write(
        JSON.stringify({ id: message.id, ok: false, error: String(error) }) +
          "\n",
      );
    }
  });
  process.stdout.write(`${backend.port}\n`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await main();
