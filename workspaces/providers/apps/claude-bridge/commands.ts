// Studio monitors use native command execution without a Codex model session.
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import { createInterface } from "node:readline";

export const commandMethods = new Set([
  "config/read",
  "command/exec",
  "command/exec/write",
  "command/exec/resize",
  "command/exec/terminate",
]);

function nativeError(value) {
  return Object.assign(new Error(value.message), value);
}

export function commandEnvironment(env, home) {
  // Do not inherit provider credentials, config overrides, or Node preload code.
  const allowed =
    /^(HOME|USER|LOGNAME|PATH|SHELL|TMPDIR|TMP|TEMP|LANG|LC_[A-Z_]+|TERM|COLORTERM|TZ|SDKROOT|DEVELOPER_DIR|SystemRoot|WINDIR)$/;
  return {
    ...Object.fromEntries(
      Object.entries(env).filter(([key]) => allowed.test(key)),
    ),
    CODEX_HOME: home,
  };
}

export function createCommandTransport({
  root,
  emit,
  env = process.env,
  stderr = (data) => process.stderr.write(data),
}) {
  const home = path.join(root, "command-executor");
  const reservations = path.join(home, "requests");
  const pending = new Map();
  const active = new Set();
  let child,
    ready,
    closed,
    closing,
    exited,
    killed = false;

  function rejectPending(error) {
    for (const waiter of pending.values()) waiter.reject(error);
    pending.clear();
  }

  function kill() {
    if (
      killed ||
      !child?.pid ||
      child.exitCode !== null ||
      child.signalCode !== null
    )
      return;
    killed = true;
    try {
      // The executor owns this group. Never signal the shared Studio backend.
      process.kill(
        process.platform === "win32" ? child.pid : -child.pid,
        "SIGKILL",
      );
    } catch (error) {
      if (error.code === "EPERM") child.kill("SIGKILL");
      else if (error.code !== "ESRCH") stderr(String(error) + "\n");
    }
  }

  function fail(error) {
    closed ||= error;
    rejectPending(closed);
    if (!child || child.exitCode !== null || child.signalCode !== null) return;
    // Native EOF cancels commands in their separate process groups. Killing
    // the executor first would discard its chance to perform that cleanup.
    child.stdin.end();
    const timer = setTimeout(kill, 1000);
    timer.unref();
    exited?.then(() => clearTimeout(timer));
  }

  function write(value) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const error = new Error(
          "Command executor input timed out; outcome unknown",
        );
        fail(error);
        reject(error);
      }, 5000);
      const complete = (error) => {
        clearTimeout(timer);
        if (error) {
          fail(
            new Error(
              "Command executor input failed; outcome unknown: " +
                error.message,
            ),
          );
          reject(closed);
        } else resolve();
      };
      try {
        child.stdin.write(JSON.stringify(value) + "\n", complete);
      } catch (error) {
        complete(error);
      }
    });
  }

  function call(method, params) {
    if (closed) return Promise.reject(closed);
    const id = "studio-command:" + randomUUID();
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      write({ id, method, params }).catch(() => {});
    });
  }

  async function start() {
    if (closed) throw closed;
    await fs.mkdir(reservations, { recursive: true, mode: 0o700 });
    if (closed) throw closed;
    child = spawn(
      env.CODEX_BIN || "codex",
      [
        "app-server",
        "--listen",
        "stdio://",
        "-c",
        'cli_auth_credentials_store="file"',
      ],
      {
        cwd: home,
        env: commandEnvironment(env, home),
        stdio: ["pipe", "pipe", "pipe"],
        detached: process.platform !== "win32",
      },
    );
    child.stderr.on("data", stderr);
    exited = new Promise((resolve) => child.once("exit", resolve));
    child.on("error", (error) =>
      fail(
        new Error("Command executor failed; outcome unknown: " + error.message),
      ),
    );
    child.stdin.on("error", (error) =>
      fail(
        new Error(
          "Command executor disconnected; outcome unknown: " + error.message,
        ),
      ),
    );
    child.on("exit", (code, signal) =>
      fail(
        new Error(
          `Command executor exited (${signal || code}); outcome unknown`,
        ),
      ),
    );
    const lines = createInterface({ input: child.stdout });
    lines.on("line", (line) => {
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        fail(new Error("Command executor sent invalid JSON; outcome unknown"));
        return;
      }
      if (message.id !== undefined && !message.method) {
        const waiter = pending.get(message.id);
        if (!waiter) return;
        pending.delete(message.id);
        if (message.error) waiter.reject(nativeError(message.error));
        else waiter.resolve(message.result);
      } else if (message.id !== undefined) {
        // No model, account, or approval request belongs to this executor.
        write({
          id: message.id,
          error: {
            code: -32601,
            message: "Unsupported command executor request",
          },
        }).catch(() => {});
      } else if (message.method === "command/exec/outputDelta") {
        emit(message.method, message.params);
      }
    });
    lines.on("close", () =>
      fail(new Error("Command executor output closed; outcome unknown")),
    );
    const timer = setTimeout(
      () =>
        fail(
          new Error(
            "Command executor initialization timed out; no command was submitted",
          ),
        ),
      20000,
    );
    try {
      await call("initialize", {
        clientInfo: { name: "studio_claude_commands", version: "1.0.0" },
        capabilities: { experimentalApi: true },
      });
      await write({ method: "initialized" });
    } finally {
      clearTimeout(timer);
    }
  }

  async function handle(method, params = {}) {
    if (!commandMethods.has(method))
      throw new Error("Unsupported command executor method: " + method);
    if (closed || closing)
      throw closed || new Error("Command executor is closing");
    if (method === "command/exec") {
      if (
        !params.sandboxPolicy ||
        typeof params.sandboxPolicy !== "object" ||
        !params.sandboxPolicy.type ||
        params.permissionProfile
      ) {
        throw new Error(
          "Studio commands require an explicit native sandboxPolicy",
        );
      }
      if (params.sandboxPolicy.type === "externalSandbox")
        throw new Error("The local command executor has no external sandbox");
      if (typeof params.processId !== "string" || !params.processId)
        throw new Error("Studio commands require a processId");
    }
    ready ||= start();
    await ready;
    if (closed || closing)
      throw closed || new Error("Command executor is closing");
    if (method !== "command/exec") return call(method, params);
    // Reserve before submission. A crash or lost reply never permits a replay.
    const file = path.join(
      reservations,
      createHash("sha256").update(params.processId).digest("hex") + ".json",
    );
    try {
      const reservation = await fs.open(file, "wx", 0o600);
      try {
        await reservation.writeFile(
          JSON.stringify({ processId: params.processId }),
        );
        await reservation.sync();
      } finally {
        await reservation.close();
      }
      const directory = await fs.open(reservations, "r");
      try {
        await directory.sync();
      } finally {
        await directory.close();
      }
    } catch (error) {
      if (error.code === "EEXIST")
        throw new Error(
          "This command processId was already submitted or reserved; do not replay it",
        );
      throw error;
    }
    if (closed || closing)
      throw (
        closed ||
        new Error("Command executor is closing; command was not submitted")
      );
    active.add(params.processId);
    try {
      return await call(method, params);
    } finally {
      active.delete(params.processId);
    }
  }

  function close() {
    if (closing) return closing;
    closing = (async () => {
      if (!closed && child) {
        let timer;
        await Promise.race([
          Promise.allSettled(
            [...active].map((processId) =>
              call("command/exec/terminate", { processId }),
            ),
          ),
          new Promise((resolve) => {
            timer = setTimeout(resolve, 1000);
          }),
        ]);
        clearTimeout(timer);
      }
      closed ||= new Error(
        "Command executor closed; pending command outcomes are unknown",
      );
      rejectPending(closed);
      if (child && child.exitCode === null && child.signalCode === null) {
        child.stdin.end();
        let timer;
        await Promise.race([
          exited,
          new Promise((resolve) => {
            timer = setTimeout(resolve, 1000);
          }),
        ]);
        clearTimeout(timer);
        kill();
      }
    })();
    return closing;
  }

  return { handle, close };
}
