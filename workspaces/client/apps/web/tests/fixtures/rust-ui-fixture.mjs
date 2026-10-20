#!/usr/bin/env node
// Browser fixture: the real `studio-runtime-server` on a disposable state
// directory with the scripted Node provider. Seeding goes through the public
// HTTP API. Usage: node rust-ui-fixture.mjs <state-dir> [--workers N]
//
// Prints the HTTP port on stdout once seeded, then serves until the process
// receives a signal (the Playwright fixture scope kills the whole group). Stdin JSON lines are test hooks:
//   {"method":"fixture/create-worker","parent":ID,"params":{...}}  POST /api/agents
// Environment:
//   CODEX_STUDIO_RUNTIME_SERVER   server binary (default: newest cargo build)
//   EXECUTION_SETTINGS_CATALOG    JSON array served by the provider's model/list
import { spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { createServer } from "node:net";
import { createInterface } from "node:readline";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

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
/** Prompt of every seeded worker. */
const WORKER_PROMPT = "Review one component";
/** Worker agents seeded under the lead when --workers is absent. */
const DEFAULT_WORKER_COUNT = 6;

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
      const { port } = probe.address();
      probe.close(() => resolvePort(port));
    });
  });
}

export async function startRustBackend(stateDir, { modelsFile } = {}) {
  const port = await freePort();
  const origin = `http://127.0.0.1:${port}`;
  // The server approves any `codex` on PATH ahead of CODEX_BIN; hide the host's.
  const path = (process.env.PATH ?? "")
    .split(delimiter)
    .filter((entry) => !existsSync(join(entry, "codex")))
    .join(delimiter);
  const child = spawn(serverBinary(), ["--port", String(port)], {
    stdio: ["ignore", 2, 2],
    env: {
      ...process.env,
      PATH: path,
      CODEX_AGENTS_STATE_DIR: stateDir,
      CODEX_BIN: fakeCodex,
      // Worker turns stay in flight (the lead's turn completes) when requested.
      ...(process.env.FIXTURE_HOLD_WORKERS === "1"
        ? { FAKE_CODEX_HOLD_PROMPT: WORKER_PROMPT }
        : {}),
      CODEX_STUDIO_WEB_DIST: dist,
      ...(modelsFile ? { FAKE_CODEX_MODELS: modelsFile } : {}),
    },
  });
  const exited = new Promise((_, reject) =>
    child.once("exit", (code, signal) =>
      reject(new Error(`studio-runtime-server exited ${code ?? signal}`)),
    ),
  );
  exited.catch(() => {});
  const deadline = Date.now() + SERVER_START_TIMEOUT_MS;
  for (;;) {
    if (child.exitCode !== null) await exited;
    try {
      if ((await fetch(`${origin}/api/session`)).ok) break;
    } catch {}
    if (Date.now() > deadline) throw new Error("server start timed out");
    await new Promise((resolveWait) => setTimeout(resolveWait, 50));
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

async function main() {
  const [stateDir, ...flags] = process.argv.slice(2);
  mkdirSync(stateDir, { recursive: true });
  const workersAt = flags.indexOf("--workers");
  const workers =
    workersAt >= 0 ? Number(flags[workersAt + 1]) : DEFAULT_WORKER_COUNT;
  let modelsFile;
  if (process.env.EXECUTION_SETTINGS_CATALOG) {
    // The state directory must stay free of foreign files.
    const scratch = mkdtempSync(join(tmpdir(), "rust-ui-fixture-"));
    process.once("exit", () =>
      rmSync(scratch, { recursive: true, force: true }),
    );
    modelsFile = join(scratch, "fake-models.json");
    writeFileSync(modelsFile, process.env.EXECUTION_SETTINGS_CATALOG);
  }
  const backend = await startRustBackend(stateDir, { modelsFile });
  const stop = () => {
    backend.child.kill("SIGTERM");
  };
  process.once("SIGTERM", stop);
  process.once("SIGINT", stop);
  backend.child.once("exit", () => process.exit(0));
  const cwd = repository;
  const lead = await backend.post("/api/agents", {
    name: "Release lead",
    prompt: "Review the release",
    cwd,
  });
  const leadId = lead.id ?? lead.agent?.id;
  if (!leadId)
    throw new Error(`Lead creation returned ${JSON.stringify(lead)}`);
  await Promise.all([
    ...Array.from({ length: workers }, (_, index) =>
      backend.post("/api/agents", {
        name: `Worker ${String(index).padStart(2, "0")}`,
        prompt: WORKER_PROMPT,
        role: "reviewer",
        parent: leadId,
        cwd,
      }),
    ),
    // A second, separate root chat for specs that need an unrelated project.
    backend.post("/api/agents", {
      name: "Other project",
      prompt: "Separate task",
      cwd,
    }),
  ]);
  createInterface({ input: process.stdin }).on("line", async (line) => {
    const message = JSON.parse(line);
    if (message.method === "fixture/create-worker")
      await backend.post("/api/agents", {
        cwd,
        ...message.params,
        parent: message.parent,
      });
    else throw new Error(`Unknown fixture hook ${message.method}`);
  });
  process.stdout.write(`${backend.port}\n`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await main();
