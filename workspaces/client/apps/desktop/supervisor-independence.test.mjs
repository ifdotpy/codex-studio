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
  symlinkSync,
  readFileSync,
  realpathSync,
  rmSync,
  existsSync,
  watch,
} from "node:fs";
import path from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { pythonResources } from "./python-resources.mjs";

const root = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
const desktopSource = path.join(root, "workspaces/client/apps/desktop");
const python = "python3";
const supervisorBinary = process.env.STUDIO_SUPERVISOR_BINARY;
const recoveryScript = path.join(desktopSource, "recover_backend.py");
const { configureRecovery, recoveryPaths, trackDesktopRecovery } =
  createRequire(import.meta.url)("./recovery.cjs");

function status(state) {
  return JSON.parse(
    execFileSync(
      python,
      ["-B", recoveryScript, "--supervisor-health", "--state", state],
      { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
    ),
  );
}

// Observe first, then inspect; passing paths advance only on fixture events.
function waitForChange(emitter, event, check, message) {
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(deadline);
      emitter.off(event, changed);
    };
    const deadline = setTimeout(() => {
      cleanup();
      reject(new Error(message));
    }, 5_000);
    function changed() {
      try {
        const value = check();
        if (value) {
          cleanup();
          resolve(value);
        }
      } catch (error) {
        cleanup();
        reject(error);
      }
    }
    emitter.on(event, changed);
    changed();
  });
}

async function stateChange(state, check, message, action = () => {}) {
  const watcher = watch(state);
  try {
    const changed = waitForChange(watcher, "change", check, message);
    action();
    return await changed;
  } finally {
    watcher.close();
  }
}

test.skipIf(!supervisorBinary)(
  "desktop and recovery lifecycle changes preserve an active Rust supervisor owner",
  async () => {
    const fixture = mkdtempSync(path.join(tmpdir(), "svo-"));
    const statePath = path.join(fixture, "state");
    mkdirSync(statePath);
    const state = realpathSync(statePath);
    const resources = path.join(fixture, "resources");
    const updatedRecoveryScript = path.join(resources, "recover_backend-v2.py");
    mkdirSync(path.join(resources, "bin"), { recursive: true });
    mkdirSync(path.join(resources, "web/dist"), { recursive: true });
    symlinkSync(
      supervisorBinary,
      path.join(resources, "bin/codex-process-supervisor"),
    );
    symlinkSync(
      process.execPath,
      path.join(resources, "bin/studio-runtime-server"),
    );
    for (const file of pythonResources)
      writeFileSync(
        path.join(resources, file),
        readFileSync(path.join(desktopSource, file)),
      );
    writeFileSync(updatedRecoveryScript, readFileSync(recoveryScript));
    writeFileSync(path.join(resources, "web/dist/index.html"), "fixture");

    let owner;
    let childPid;
    let socket;
    let recoveryProcess;
    let backendServer;
    try {
      owner = spawn(supervisorBinary, ["--state", state], {
        stdio: "ignore",
        env: {
          ...process.env,
          CODEX_AGENTS_STATE_DIR: state,
          CODEX_AGENTS_SUPERVISOR_MODE: "1",
        },
      });
      const initialHealth = await stateChange(
        state,
        () => existsSync(path.join(state, "supervisor.sock")) && status(state),
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
          protocol: initialHealth.protocol,
          stateDir: state,
          backendId: "private-fixture-backend",
        })}\n`,
      );
      assert.deepEqual(await nextMessage(), {
        ok: true,
        protocol: initialHealth.protocol,
      });
      socket.write(
        `${JSON.stringify({
          requestId: 1,
          handle: "fixture:live-child",
          action: "open",
          command: [
            process.execPath,
            "-e",
            "process.stdin.pipe(process.stdout)",
          ],
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
        CODEX_BIN: process.execPath,
        PATH: process.env.PATH,
      };
      backendServer = createServer((_request, response) => {
        response.setHeader("content-type", "application/json");
        response.end(
          JSON.stringify({
            application: "codex-agents",
            backendKind: "rust",
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
      await configureRecovery({
        ...appArgs,
        supervisor: updatedRecoveryScript,
      });
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

      const recoveryConfig = path.join(state, "background-recovery.json");
      recoveryProcess = spawn(
        python,
        [
          "-B",
          path.join(desktopSource, "recover_backend.py"),
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
        await waitForChange(
          recoveryProcess.stdout,
          "data",
          () => recoveryOutput.includes("supervisor ready"),
          "recovery did not attach to the independent owner",
        );
      } catch (error) {
        let log = "";
        try {
          log = readFileSync(
            path.join(state, "background-recovery.log"),
            "utf8",
          );
        } catch {}
        throw new Error(
          `${error.message}; exit=${recoveryProcess.exitCode}; stdout/stderr=${recoveryOutput}; log=${log}`,
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
      assert.equal(
        JSON.parse(readFileSync(desktop.filename)).desiredOpen,
        false,
      );
      const afterQuit = status(state);
      assert.equal(
        JSON.parse(readFileSync(path.join(state, "supervisor.lock"), "utf8"))
          .pid,
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
      if (childPid) {
        await stateChange(
          state,
          () => {
            try {
              process.kill(childPid, 0);
              return false;
            } catch (error) {
              if (error.code === "ESRCH") return true;
              throw error;
            }
          },
          "fake child did not exit",
          () => {
            try {
              process.kill(childPid, "SIGTERM");
            } catch (error) {
              if (error.code !== "ESRCH") throw error;
            }
          },
        );
      }
      socket?.destroy();
      if (owner && owner.exitCode === null) {
        owner.kill("SIGTERM");
        await new Promise((resolve) => owner.once("exit", resolve));
      }
      rmSync(fixture, { recursive: true, force: true, maxRetries: 10 });
    }
  },
);
