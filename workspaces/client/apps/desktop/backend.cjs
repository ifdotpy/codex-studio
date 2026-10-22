const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { createHash } = require("node:crypto");
const { spawn } = require("node:child_process");
const { savedLaunchEnvironment } = require("./restart-environment.cjs");
// Desktop wire contract; Rust startup.rs is checked for parity in backend.test.mjs.
const desktopProtocolVersion = 1;
const backendBuildHexLength = 64;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function backendExitStatus(child) {
  if (child.exitCode !== null) return child.exitCode;
  if (child.signalCode !== null) return child.signalCode;
  return null;
}

function runtimeServer(resources, env = process.env) {
  const configured = env.CODEX_RUNTIME_SERVER;
  const candidates = configured
    ? [configured]
    : [
        path.join(
          resources,
          "bin",
          process.platform === "win32"
            ? "studio-runtime-server.exe"
            : "studio-runtime-server",
        ),
        path.join(
          __dirname,
          "../../../../target/release",
          process.platform === "win32"
            ? "studio-runtime-server.exe"
            : "studio-runtime-server",
        ),
        path.join(
          __dirname,
          "../../../../target/debug",
          process.platform === "win32"
            ? "studio-runtime-server.exe"
            : "studio-runtime-server",
        ),
      ];
  for (const candidate of candidates) {
    if (!path.isAbsolute(candidate)) continue;
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {}
  }
  throw new Error(
    configured
      ? "CODEX_RUNTIME_SERVER must point to an executable runtime server."
      : "The Rust runtime server is missing. Build or package studio-runtime-server, or set CODEX_RUNTIME_SERVER to its absolute executable path.",
  );
}

function runtimeServerBuild(resources, env = process.env) {
  const file = runtimeServer(resources, env);
  const descriptor = fs.openSync(file, "r");
  try {
    const hash = createHash("sha256");
    const buffer = Buffer.allocUnsafe(64 * 1024);
    let bytesRead;
    while (
      (bytesRead = fs.readSync(descriptor, buffer, 0, buffer.length, null)) > 0
    ) {
      hash.update(buffer.subarray(0, bytesRead));
    }
    return hash.digest("hex");
  } finally {
    fs.closeSync(descriptor);
  }
}

function backendBuild(resources, env = process.env) {
  return runtimeServerBuild(resources, env);
}
function updateStatus(resources, running, env = process.env) {
  let availableBackendBuild;
  try {
    availableBackendBuild = backendBuild(resources, env);
  } catch (error) {
    console.error(
      "Cannot compute the installed backend identity:",
      error.stderr?.toString().trim() || error.message,
    );
    return { availableBackendBuild: null, updateRequired: null };
  }
  return {
    availableBackendBuild,
    updateRequired: running.backendBuild !== availableBackendBuild,
  };
}
function stateDirectory(env = process.env) {
  return path.resolve(
    env.CODEX_AGENTS_STATE_DIR ||
      path.join(
        env.XDG_STATE_HOME || path.join(os.homedir(), ".local/state"),
        "codex-agents",
      ),
  );
}
function executable(name, env = process.env) {
  const configured = name === "codex" ? env.CODEX_BIN : undefined;
  const candidates = configured
    ? [configured]
    : (env.PATH || "")
        .split(path.delimiter)
        .concat(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"])
        .map((dir) => path.join(dir, name));
  for (const file of candidates) {
    if (!path.isAbsolute(file)) continue;
    try {
      fs.accessSync(file, fs.constants.X_OK);
      return file;
    } catch {}
  }
  throw new Error(
    `Install ${name}, or set ${name === "codex" ? "CODEX_BIN" : "PATH"} to its absolute executable path.`,
  );
}
async function identity(
  origin,
  state,
  {
    attemptTimeoutMs = 5000,
    timeoutMs = 15000,
    retryDelayMs = 200,
    expectedBackendBuild,
  } = {},
) {
  const deadline = performance.now() + timeoutMs;
  let uncertain = false;
  let response, data;
  for (;;) {
    const remaining = Math.max(1, Math.ceil(deadline - performance.now()));
    const signal = AbortSignal.timeout(Math.min(attemptTimeoutMs, remaining));
    try {
      response = await fetch(`${origin}/api/desktop`, {
        signal,
        redirect: "error",
      });
      try {
        data = await response.json();
      } catch (error) {
        if (signal.aborted) throw error;
        throw new Error(
          "Port is occupied by an incompatible server. Update or stop that server first.",
        );
      }
      break;
    } catch (error) {
      const refused = error.cause?.code === "ECONNREFUSED";
      if (refused && !uncertain) return null;
      if (!signal.aborted && !refused)
        throw new Error(`Cannot verify the local backend: ${error.message}`);
      // A timeout does not prove that the port or state directory is available.
      uncertain = true;
      const remaining = deadline - performance.now();
      if (remaining <= 0)
        throw new Error(
          "The local backend did not confirm its identity in time. Try opening Studio again. No replacement backend was started.",
        );
      await sleep(Math.min(retryDelayMs, remaining));
    }
  }
  if (
    !response.ok ||
    data.application !== "codex-agents" ||
    data.protocol !== desktopProtocolVersion ||
    data.backendKind !== "rust" ||
    typeof data.backendBuild !== "string" ||
    data.backendBuild.length !== backendBuildHexLength ||
    !/^[a-f0-9]+$/.test(data.backendBuild) ||
    !Number.isSafeInteger(data.pid) ||
    data.pid < 1 ||
    data.stateDir !== state
  ) {
    throw new Error(
      "The existing server is incompatible or uses a different state directory. Update or stop it before desktop startup.",
    );
  }
  if (
    expectedBackendBuild !== undefined &&
    data.backendBuild !== expectedBackendBuild
  )
    throw new Error(
      "The started Rust backend identity does not match the selected executable.",
    );
  process.kill(data.pid, 0);
  return data;
}
async function ensureBackend({ resources, port = 4620, env = process.env }) {
  if (!Number.isInteger(port) || port < 1024 || port > 65535)
    throw new Error("Backend port must be between 1024 and 65535.");
  const state = stateDirectory(env);
  fs.mkdirSync(state, { recursive: true });
  const canonicalState = fs.realpathSync(state);
  let persistedSupervisorMode = false;
  let saved = {};
  try {
    saved = JSON.parse(
      fs.readFileSync(
        path.join(canonicalState, "background-recovery.json"),
        "utf8",
      ),
    );
    if (
      saved.supervisorEnabled !== undefined &&
      typeof saved.supervisorEnabled !== "boolean"
    )
      throw new Error("The saved supervisor setting is invalid.");
    persistedSupervisorMode = saved.supervisorEnabled === true;
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  const supervisorMode =
    env.CODEX_AGENTS_SUPERVISOR_MODE === undefined
      ? persistedSupervisorMode
      : env.CODEX_AGENTS_SUPERVISOR_MODE === "1";
  const origin = `http://127.0.0.1:${port}`;
  const existing = await identity(origin, canonicalState);
  if (
    existing &&
    supervisorMode &&
    existing.supervisorMode !== true &&
    existing.supervisorFallback !== true
  )
    throw new Error(
      "Supervisor mode is enabled, but the running backend does not use it. Use the coordinated restart preflight before attaching.",
    );
  if (existing)
    return {
      ...existing,
      ...updateStatus(resources, existing, env),
      origin,
      owned: false,
    };
  const launchEnv = savedLaunchEnvironment(env, saved, canonicalState);
  const packagedRenderer = path.join(resources, "web/dist");
  const developmentRenderer = path.join(
    resources,
    "workspaces/client/apps/web/dist",
  );
  const renderer = fs.existsSync(path.join(developmentRenderer, "index.html"))
    ? developmentRenderer
    : packagedRenderer;
  if (!fs.existsSync(path.join(renderer, "index.html")))
    throw new Error(
      "Desktop assets are missing. Run the web build before starting or packaging desktop.",
    );
  const codex = executable("codex", launchEnv);
  const log = path.join(state, "canvas.log");
  const backendExecutable = runtimeServer(resources, launchEnv);
  const backendArgs = ["--port", String(port)];
  const backendBuildId = runtimeServerBuild(resources, launchEnv);
  // The Rust server records itself as the state owner (rust-state-owner.json)
  // after it holds runtime.lock and before its first write.
  const fd = fs.openSync(log, "a", 0o600);
  let child;
  try {
    child = spawn(backendExecutable, backendArgs, {
      cwd: os.homedir(),
      detached: true,
      stdio: ["ignore", fd, fd],
      env: {
        ...launchEnv,
        CODEX_AGENTS_STATE_DIR: canonicalState,
        CODEX_AGENTS_SUPERVISOR_MODE: supervisorMode ? "1" : "0",
        CODEX_BACKEND_BUILD: backendBuildId,
        CODEX_BIN: codex,
        CODEX_NODE: process.execPath,
        PATH: `${path.dirname(codex)}:${launchEnv.PATH || "/usr/bin:/bin"}`,
      },
    });
  } finally {
    fs.closeSync(fd);
  }
  let failure;
  child.on("error", (error) => {
    failure = error;
  });
  child.unref();
  for (let attempt = 0; attempt < 100; attempt++) {
    await sleep(100);
    if (failure) throw failure;
    const ready = await identity(origin, canonicalState, {
      expectedBackendBuild: backendBuildId,
    });
    if (ready) {
      // A competing launcher can win the port. Its runtime flock remains authoritative.
      fs.writeFileSync(path.join(state, "canvas.pid"), `${ready.pid}\n`, {
        mode: 0o600,
      });
      return {
        ...ready,
        ...updateStatus(resources, ready, env),
        origin,
        owned: ready.pid === child.pid,
      };
    }
    const exitStatus = backendExitStatus(child);
    if (exitStatus !== null)
      throw new Error(
        `Backend exited (${exitStatus}). Read ${log}. Another runtime may own this state directory.`,
      );
  }
  // The detached runtime may still be starting. Never kill it or launch a replacement.
  throw new Error(
    `Backend startup has not completed. Read ${log}. The process is ${child.pid}.`,
  );
}
module.exports = {
  ensureBackend,
  identity,
  executable,
  stateDirectory,
  backendBuild,
  updateStatus,
  backendExitStatus,
  runtimeServer,
  runtimeServerBuild,
  desktopProtocolVersion,
  backendBuildHexLength,
};
