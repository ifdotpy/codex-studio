// Start the packaged host hidden with isolated client and backend state.
import assert from "node:assert/strict";
import type { ChildProcess } from "node:child_process";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { open, mkdtemp, realpath, rm, stat } from "node:fs/promises";
import { createServer } from "node:net";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { identity } from "../desktop/backend.cjs";

const startupTimeoutMs = 60_000;
const startupPollMs = 100;
const hostStopTimeoutMs = 10_000;
const application = process.argv[2];
assert.ok(application, "Supply the packaged application path.");
const bundle = path.resolve(application);
const executableName = execFileSync(
  "plutil",
  [
    "-extract",
    "CFBundleExecutable",
    "raw",
    "-o",
    "-",
    path.join(bundle, "Contents/Info.plist"),
  ],
  { encoding: "utf8" },
).trim();
assert.equal(path.basename(executableName), executableName);
const fixture = await realpath(
  await mkdtemp(path.join(os.tmpdir(), "studio-native-pkg-")),
);
const state = path.join(fixture, "state");
const listener = createServer();
listener.listen(0, "127.0.0.1");
await once(listener, "listening");
const address = listener.address();
assert.ok(address && typeof address !== "string");
const port = address.port;
await new Promise<void>((resolve, reject) =>
  listener.close((error) => (error ? reject(error) : resolve())),
);
const log = await open(path.join(fixture, "host.log"), "w");
let host: ChildProcess | undefined;
let ownedPid;
try {
  host = spawn(
    path.join(bundle, "Contents/MacOS", executableName),
    ["--hidden", "-ApplePersistenceIgnoreState", "YES"],
    {
      env: {
        ...process.env,
        CODEX_AGENTS_STATE_DIR: state,
        CODEX_DESKTOP_PROFILE: path.join(fixture, "profile"),
        CODEX_DESKTOP_EPHEMERAL_PROFILE: "1",
        CODEX_DESKTOP_PORT: String(port),
        CODEX_AGENTS_SUPERVISOR_MODE: "0",
        CODEX_AGENTS_PYTHON: "/nonexistent",
      },
      stdio: ["ignore", log.fd, log.fd],
    },
  );
  let failure;
  host.once("error", (error) => {
    failure = error;
  });
  const deadline = performance.now() + startupTimeoutMs;
  let backend;
  while (performance.now() < deadline) {
    if (failure) throw failure;
    assert.equal(
      host.exitCode,
      null,
      `Packaged host exited with ${host.exitCode}`,
    );
    backend = await identity(`http://127.0.0.1:${port}`, state);
    if (backend) break;
    await delay(startupPollMs);
  }
  assert.ok(
    backend,
    "The packaged host did not start its backend before the deadline",
  );
  assert.equal(backend.backendKind, "rust");
  const command = execFileSync(
    "/bin/ps",
    ["-p", String(backend.pid), "-o", "command="],
    { encoding: "utf8" },
  );
  assert.ok(
    command.includes(
      path.join(
        bundle,
        "Contents/Resources/workspace/bin/studio-runtime-server",
      ),
    ),
    "Backend did not start from this bundle",
  );
  ownedPid = backend.pid;
  assert.ok(
    (await stat(path.join(fixture, "profile/client/local"))).isDirectory(),
  );
  console.log(
    JSON.stringify({
      result: "PASS",
      bundle,
      backendPid: ownedPid,
      stateDir: backend.stateDir,
      checks:
        "hidden packaged host started its bundled Rust backend with Python unavailable",
    }),
  );
} finally {
  if (host?.pid && host.exitCode === null && host.signalCode === null) {
    const child = host;
    const exited = once(child, "exit");
    host.kill("SIGTERM");
    let timer;
    try {
      await Promise.race([
        exited,
        new Promise((resolve) => {
          timer = setTimeout(() => {
            child.kill("SIGKILL");
            resolve(exited);
          }, hostStopTimeoutMs);
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }
  if (ownedPid) {
    try {
      process.kill(ownedPid, "SIGTERM");
    } catch (error) {
      if (
        !(error instanceof Error && "code" in error && error.code === "ESRCH")
      )
        console.error(
          "Cannot stop the fixture backend:",
          error instanceof Error ? error.message : String(error),
        );
    }
  }
  await log.close();
  await rm(fixture, { recursive: true, force: true });
}
