const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { createHash } = require("node:crypto");
const { spawn, execFileSync } = require("node:child_process");
const { savedLaunchEnvironment } = require("./restart-environment.cjs");
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function backendExitStatus(child) {
  if (child.exitCode !== null) return child.exitCode;
  if (child.signalCode !== null) return child.signalCode;
  return null;
}

const backendIdentityTimeoutMs = 10000;

function backendBuild(resources, env = process.env) {
  const developmentIdentity = path.join(
    resources,
    "workspaces/runtime/apps/server/src/codex_backend_identity.py",
  );
  const identityScript = fs.existsSync(developmentIdentity)
    ? developmentIdentity
    : path.join(resources, "scripts/codex_backend_identity.py");
  const identityInfo = fs.lstatSync(identityScript);
  if (identityInfo.isSymbolicLink() || !identityInfo.isFile())
    throw new Error("The backend identity helper must be a regular file.");
  const output = execFileSync(
    executable("python3", env),
    ["-B", identityScript],
    { encoding: "utf8", timeout: backendIdentityTimeoutMs, env },
  ).trim();
  if (!/^[a-f0-9]{64}$/.test(output))
    throw new Error(
      "The backend identity helper returned an invalid identity.",
    );
  return output;
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
  const configured =
    name === "python3" ? env.CODEX_AGENTS_PYTHON : env.CODEX_BIN;
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
      if (name === "python3")
        execFileSync(
          file,
          [
            "-c",
            "import sys; sys.exit(0 if sys.version_info >= (3,11) else 1)",
          ],
          { timeout: 5000, stdio: "ignore" },
        );
      return file;
    } catch {}
  }
  throw new Error(
    `Install ${name}, or set ${name === "python3" ? "CODEX_AGENTS_PYTHON" : "CODEX_BIN"} to its absolute executable path.`,
  );
}
function apiPython(resources, env = process.env) {
  const hasApi = (file) => {
    try {
      execFileSync(
        file,
        [
          "-c",
          "import sys; assert sys.version_info >= (3,11); import fastapi, httpx, pydantic, uvicorn",
        ],
        { timeout: 10000, stdio: "ignore" },
      );
      return true;
    } catch {
      return false;
    }
  };
  const configured = env.CODEX_AGENTS_PYTHON;
  if (configured) {
    if (hasApi(configured)) return configured;
    throw new Error(
      "CODEX_AGENTS_PYTHON must point to Python 3.11+ with FastAPI, Pydantic, Uvicorn, and HTTPX from requirements.txt installed.",
    );
  }

  const requirements = path.join(resources, "requirements.txt");
  if (fs.existsSync(requirements)) {
    const digest = createHash("sha256")
      .update(fs.readFileSync(requirements))
      .digest("hex");
    const cacheRoot =
      env.XDG_CACHE_HOME ||
      (process.platform === "darwin"
        ? path.join(os.homedir(), "Library", "Caches")
        : path.join(os.homedir(), ".cache"));
    const managed = path.join(
      cacheRoot,
      "codex-agents",
      "python",
      digest,
      "bin",
      "python",
    );
    if (hasApi(managed)) return managed;
  }

  const fallbackEnv = { ...env };
  delete fallbackEnv.CODEX_AGENTS_PYTHON;
  try {
    const fallback = executable("python3", fallbackEnv);
    if (hasApi(fallback)) return fallback;
  } catch {}
  throw new Error(
    "No Python 3.11+ interpreter has FastAPI, Pydantic, Uvicorn, and HTTPX. Run `python3 workspaces/runtime/apps/server/src/install-cli.py` to prepare the managed environment, or set CODEX_AGENTS_PYTHON to an equipped interpreter.",
  );
}
async function identity(
  origin,
  state,
  { attemptTimeoutMs = 5000, timeoutMs = 15000, retryDelayMs = 200 } = {},
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
    data.protocol !== 1 ||
    typeof data.backendBuild !== "string" ||
    !/^[a-f0-9]{64}$/.test(data.backendBuild) ||
    !Number.isSafeInteger(data.pid) ||
    data.pid < 1 ||
    data.stateDir !== state
  ) {
    throw new Error(
      "The existing server is incompatible or uses a different state directory. Update or stop it before desktop startup.",
    );
  }
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
  const packagedScripts = path.join(resources, "scripts");
  const developmentScripts = path.join(
    resources,
    "workspaces/runtime/apps/server/src",
  );
  const scripts = fs.existsSync(path.join(developmentScripts, "codex-canvas"))
    ? developmentScripts
    : packagedScripts;
  const packagedRenderer = path.join(resources, "web/dist");
  const developmentRenderer = path.join(
    resources,
    "workspaces/client/apps/web/dist",
  );
  const renderer = fs.existsSync(path.join(developmentRenderer, "index.html"))
    ? developmentRenderer
    : packagedRenderer;
  const script = path.join(scripts, "codex-canvas");
  if (
    !fs.existsSync(script) ||
    !fs.existsSync(path.join(renderer, "index.html"))
  )
    throw new Error(
      "Desktop assets are missing. Run the web build before starting or packaging desktop.",
    );
  const python = apiPython(resources, launchEnv);
  execFileSync(
    python,
    [
      "-c",
      'import sys; assert sys.version_info >= (3,11), "Python 3.11 or later is required"',
    ],
    { timeout: 5000 },
  );
  const codex = executable("codex", launchEnv);
  if (supervisorMode) {
    const supervisor = path.join(scripts, "codex_process_supervisor.py");
    if (!fs.existsSync(supervisor))
      throw new Error(
        "Supervisor mode is enabled but its packaged script is missing.",
      );
    const deadline = Date.now() + 10000;
    let lastError;
    while (Date.now() < deadline) {
      try {
        execFileSync(
          python,
          ["-B", supervisor, "--check", "--state", canonicalState],
          {
            timeout: 2000,
            stdio: "ignore",
            env: { ...launchEnv, CODEX_AGENTS_STATE_DIR: canonicalState },
          },
        );
        lastError = null;
        break;
      } catch (error) {
        lastError = error;
        await sleep(100);
      }
    }
    if (lastError) {
      throw new Error(
        `Supervisor mode is enabled but no compatible supervisor is ready for ${canonicalState}. The backend was not started: ${lastError.message}`,
      );
    }
  }
  const log = path.join(state, "canvas.log");
  const fd = fs.openSync(log, "a", 0o600);
  let child;
  try {
    child = spawn(python, ["-B", script, "--port", String(port)], {
      cwd: os.homedir(),
      detached: true,
      stdio: ["ignore", fd, fd],
      env: {
        ...launchEnv,
        CODEX_AGENTS_STATE_DIR: canonicalState,
        CODEX_AGENTS_SUPERVISOR_MODE: supervisorMode ? "1" : "0",
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
    const ready = await identity(origin, canonicalState);
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
  apiPython,
  stateDirectory,
  backendBuild,
  updateStatus,
  backendExitStatus,
};
