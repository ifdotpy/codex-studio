import assert from "node:assert/strict";
import { test } from "vitest";
import { spawn, execFileSync } from "node:child_process";
import { createConnection } from "node:net";
import { createServer } from "node:http";
import { EventEmitter } from "node:events";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  realpathSync,
  rmSync,
} from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const python = execFileSync(
  "python3",
  ["-c", "import sys; print(sys.executable)"],
  {
    encoding: "utf8",
  },
).trim();
const supervisorScript = path.join(root, "scripts/codex_process_supervisor.py");
const { configureRecovery, recoveryPaths, trackDesktopRecovery } =
  createRequire(import.meta.url)("./recovery.cjs");

function status(state) {
  return JSON.parse(
    execFileSync(
      python,
      ["-B", supervisorScript, "--status-json", "--state", state],
      { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
    ),
  );
}

function waitFor(check, message, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const poll = () => {
      try {
        const value = check();
        if (value) return resolve(value);
      } catch {}
      if (Date.now() >= deadline) return reject(new Error(message));
      setTimeout(poll, 25);
    };
    poll();
  });
}

test("desktop and recovery lifecycle changes preserve an active supervisor owner", async () => {
  const fixture = mkdtempSync("/tmp/svo-");
  const statePath = path.join(fixture, "state");
  mkdirSync(statePath);
  const state = realpathSync(statePath);
  const resources = path.join(fixture, "resources");
  const recoveryScript = path.join(resources, "recover_backend.py");
  const updatedRecoveryScript = path.join(resources, "recover_backend-v2.py");
  mkdirSync(path.join(resources, "scripts"), { recursive: true });
  mkdirSync(path.join(resources, "web/dist"), { recursive: true });
  writeFileSync(path.join(resources, "scripts/codex-canvas"), "fixture");
  writeFileSync(recoveryScript, "fixture v1");
  writeFileSync(updatedRecoveryScript, "fixture v2");
  writeFileSync(
    path.join(resources, "scripts/codex_process_supervisor.py"),
    readFileSync(supervisorScript),
  );
  writeFileSync(path.join(resources, "web/dist/index.html"), "fixture");

  let owner;
  let childPid;
  let socket;
  let recoveryProcess;
  let backendServer;
  try {
    owner = spawn(python, ["-B", supervisorScript, "--state", state], {
      stdio: "ignore",
      env: {
        ...process.env,
        CODEX_AGENTS_STATE_DIR: state,
        CODEX_AGENTS_SUPERVISOR_MODE: "1",
      },
    });
    const initialHealth = await waitFor(
      () => status(state),
      "owner did not start",
    );
    const ownerPid = JSON.parse(
      readFileSync(path.join(state, "supervisor.lock"), "utf8"),
    ).pid;

    socket = createConnection(path.join(state, "supervisor.sock"));
    let buffered = "";
    const messages = [];
    const waiters = [];
    socket.on("data", (chunk) => {
      buffered += chunk.toString();
      for (;;) {
        const end = buffered.indexOf("\n");
        if (end < 0) break;
        const line = buffered.slice(0, end);
        buffered = buffered.slice(end + 1);
        const waiter = waiters.shift();
        if (waiter) waiter(JSON.parse(line));
        else messages.push(JSON.parse(line));
      }
    });
    const nextMessage = () =>
      messages.length
        ? Promise.resolve(messages.shift())
        : new Promise((resolve) => waiters.push(resolve));
    await new Promise((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    socket.write(
      `${JSON.stringify({
        protocol: 1,
        stateDir: state,
        backendId: "private-fixture-backend",
      })}\n`,
    );
    assert.equal((await nextMessage()).ok, true);
    const childCode =
      "import time; print('fake child ready', flush=True); time.sleep(120)";
    socket.write(
      `${JSON.stringify({
        requestId: 1,
        handle: "fixture:live-child",
        action: "open",
        command: [python, "-u", "-c", childCode],
        env: { ...process.env },
        cwd: fixture,
      })}\n`,
    );
    const opened = await nextMessage();
    assert.equal(opened.requestId, 1);
    assert.equal(opened.error, undefined);
    childPid = status(state).handles[0].pid;
    assert.equal(initialHealth.handles.length, 0);
    assert.equal(status(state).handles[0].pid, childPid);

    const files = recoveryPaths({ CODEX_AGENTS_STATE_DIR: state }, fixture);
    const registered = new Set([
      `gui/501/${files.label}`,
      `gui/501/${files.supervisorLabel}`,
    ]);
    const calls = [];
    const run = async (_file, args) => {
      calls.push(args);
      const [action, target] = args;
      if (action === "print" && !registered.has(target))
        throw new Error("service is not loaded");
      if (action === "bootout") registered.delete(target);
      if (action === "bootstrap")
        registered.add(
          args[2] === files.supervisorPlist
            ? `gui/501/${files.supervisorLabel}`
            : `gui/501/${files.label}`,
        );
    };
    const env = {
      CODEX_AGENTS_STATE_DIR: state,
      CODEX_AGENTS_SUPERVISOR_MODE: "1",
      CODEX_BIN: "/usr/bin/true",
      PATH: process.env.PATH,
    };
    backendServer = createServer((_request, response) => {
      response.setHeader("content-type", "application/json");
      response.end(
        JSON.stringify({
          application: "codex-agents",
          protocol: 1,
          stateDir: state,
          pid: process.pid,
          supervisorMode: true,
        }),
      );
    });
    await new Promise((resolve) =>
      backendServer.listen(0, "127.0.0.1", resolve),
    );
    const port = backendServer.address().port;
    const appArgs = {
      applicationPath:
        "/Applications/Codex Studio.app/Contents/Resources/app.asar",
      resources,
      supervisor: recoveryScript,
      enabled: true,
      port,
      env,
      home: fixture,
      uid: 501,
      run,
    };
    await configureRecovery(appArgs);
    await configureRecovery({ ...appArgs, supervisor: updatedRecoveryScript });
    assert.ok(
      calls.some(
        ([action, target]) =>
          action === "bootout" && target === `gui/501/${files.label}`,
      ),
      "the changed recovery plist should replace its own service",
    );
    assert.ok(
      calls.every(
        ([action, target]) =>
          !["bootout", "kickstart"].includes(action) ||
          target !== `gui/501/${files.supervisorLabel}`,
      ),
      "recovery updates must not stop or kick the supervisor service",
    );

    const probeScript = `
import importlib.util, pathlib, subprocess
from unittest.mock import patch
spec = importlib.util.spec_from_file_location('recovery', ${JSON.stringify(path.join(root, "desktop/recover_backend.py"))})
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
config = {'supervisorEnabled': True, 'python': ${JSON.stringify(python)}, 'codex': '/usr/bin/true', 'resources': ${JSON.stringify(resources)}}
with patch.object(module.subprocess, 'run', side_effect=subprocess.TimeoutExpired(['probe'], 2)), patch.object(module.subprocess, 'Popen') as spawn:
    child, result = module.supervisor_tick(config, pathlib.Path(${JSON.stringify(state)}))
    assert child is None and result == 'supervisor starting'
    spawn.assert_not_called()
`;
    execFileSync(python, ["-B", "-c", probeScript], { stdio: "pipe" });

    const recoveryConfig = path.join(state, "background-recovery.json");
    const savedConfig = JSON.parse(readFileSync(recoveryConfig, "utf8"));
    let probeDiagnostic = "status probe succeeded";
    try {
      execFileSync(
        python,
        ["-B", supervisorScript, "--status-json", "--state", state],
        {
          encoding: "utf8",
          env: {
            ...process.env,
            ...savedConfig.environment,
            CODEX_AGENTS_STATE_DIR: state,
            CODEX_BIN: savedConfig.codex,
            CODEX_AGENTS_SUPERVISOR_MODE: "1",
          },
        },
      );
    } catch (error) {
      probeDiagnostic = error.stderr?.toString() || error.message;
    }
    recoveryProcess = spawn(
      python,
      [
        "-B",
        path.join(root, "desktop/recover_backend.py"),
        "--config",
        recoveryConfig,
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    let recoveryOutput = "";
    recoveryProcess.stdout.on("data", (chunk) => {
      recoveryOutput += chunk.toString();
    });
    recoveryProcess.stderr.on("data", (chunk) => {
      recoveryOutput += chunk.toString();
    });
    try {
      await waitFor(
        () => recoveryOutput.includes("supervisor ready"),
        "recovery did not attach to the independent owner",
      );
    } catch (error) {
      let log = "";
      try {
        log = readFileSync(path.join(state, "background-recovery.log"), "utf8");
      } catch {}
      throw new Error(
        `${error.message}; exit=${recoveryProcess.exitCode}; stdout/stderr=${recoveryOutput}; log=${log}; probe=${probeDiagnostic}`,
      );
    }
    // A launchctl kickstart -k replaces only the recovery job process.
    recoveryProcess.kill("SIGTERM");
    await new Promise((resolve) => recoveryProcess.once("exit", resolve));
    recoveryProcess = null;

    const app = new EventEmitter();
    app.getPath = () => path.join(fixture, "profile");
    const desktop = trackDesktopRecovery({
      app,
      env: { CODEX_AGENTS_STATE_DIR: state },
    });
    desktop.closeExplicitly();
    app.emit("before-quit");
    assert.equal(JSON.parse(readFileSync(desktop.filename)).desiredOpen, false);
    const afterQuit = status(state);
    assert.equal(
      JSON.parse(readFileSync(path.join(state, "supervisor.lock"), "utf8")).pid,
      ownerPid,
    );
    assert.equal(afterQuit.handles[0].pid, childPid);
    assert.equal(afterQuit.recovery.blocked, null);
  } finally {
    if (recoveryProcess && recoveryProcess.exitCode === null) {
      recoveryProcess.kill("SIGTERM");
      await new Promise((resolve) => recoveryProcess.once("exit", resolve));
    }
    if (backendServer)
      await new Promise((resolve) => backendServer.close(resolve));
    socket?.destroy();
    if (childPid) {
      try {
        process.kill(childPid, "SIGTERM");
      } catch {}
      await waitFor(() => {
        try {
          execFileSync("/bin/ps", ["-p", String(childPid)], {
            stdio: "ignore",
          });
          return false;
        } catch {
          return true;
        }
      }, "fake child did not exit");
    }
    if (owner && owner.exitCode === null) {
      owner.kill("SIGTERM");
      await new Promise((resolve) => owner.once("exit", resolve));
    }
    rmSync(fixture, { recursive: true, force: true, maxRetries: 10 });
  }
});
