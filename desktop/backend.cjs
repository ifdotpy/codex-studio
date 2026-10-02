const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { createHash } = require("node:crypto");
const { spawn, execFileSync } = require("node:child_process");
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const excludedSourceDirectories = new Set([
  "tests",
  "benchmarks",
  "__pycache__",
  "vendor",
  "venv",
]);

function backendSources(scripts) {
  const root = fs.realpathSync(scripts);
  const found = new Map();
  const relativeName = (file) => {
    const relative = path.relative(root, file).split(path.sep).join("/");
    const parts = relative.split("/");
    if (
      !relative ||
      relative.includes("\\") ||
      path.isAbsolute(relative) ||
      parts.some((part) => part === "" || part === "." || part === "..")
    )
      throw new Error(`Unsafe backend source path: ${relative}`);
    return relative;
  };
  const add = (file) => {
    const relative = relativeName(file);
    const info = fs.lstatSync(file);
    if (info.isSymbolicLink() || !info.isFile())
      throw new Error(
        `Backend source must be a local regular file: ${relative}`,
      );
    found.set(relative, file);
  };
  const symlinkIsDirectory = (file) => {
    try {
      return fs.statSync(file).isDirectory();
    } catch (error) {
      if (error.code === "ENOENT") return false;
      throw error;
    }
  };
  const isPackage = (directory) => {
    const initializer = path.join(directory, "__init__.py");
    try {
      const info = fs.lstatSync(initializer);
      if (info.isSymbolicLink() || !info.isFile())
        throw new Error(
          `Backend package initializer must be a regular file: ${relativeName(initializer)}`,
        );
      return true;
    } catch (error) {
      if (error.code === "ENOENT") return false;
      throw error;
    }
  };
  const visitPackage = (directory) => {
    const info = fs.lstatSync(directory);
    if (info.isSymbolicLink() || !info.isDirectory())
      throw new Error(
        `Backend package must be a local directory: ${relativeName(directory)}`,
      );
    for (const name of fs.readdirSync(directory).sort()) {
      if (excludedSourceDirectories.has(name)) continue;
      const file = path.join(directory, name);
      const child = fs.lstatSync(file);
      if (child.isSymbolicLink()) {
        if (
          name.endsWith(".py") ||
          name === "codex_federation_crypto.mjs" ||
          symlinkIsDirectory(file)
        )
          throw new Error(
            `Backend source must not be a symlink: ${relativeName(file)}`,
          );
      } else if (child.isFile() && name.endsWith(".py")) {
        add(file);
      } else if (child.isDirectory() && isPackage(file)) {
        visitPackage(file);
      }
    }
  };

  for (const name of fs.readdirSync(root).sort()) {
    if (excludedSourceDirectories.has(name)) continue;
    const file = path.join(root, name);
    const info = fs.lstatSync(file);
    if (info.isSymbolicLink()) {
      if (name.endsWith(".py") || symlinkIsDirectory(file))
        throw new Error(
          `Backend source must not be a symlink: ${relativeName(file)}`,
        );
    } else if (
      info.isFile() &&
      (name.endsWith(".py") ||
        name === "codex-canvas" ||
        name === "codex_federation_crypto.mjs")
    ) {
      add(file);
    } else if (info.isDirectory() && isPackage(file)) {
      visitPackage(file);
    }
  }
  return [...found.entries()].sort(([left], [right]) =>
    left < right ? -1 : left > right ? 1 : 0,
  );
}

function backendBuild(resources) {
  const scripts = path.join(resources, "scripts");
  const sources = backendSources(scripts);
  if (!sources.some(([name]) => name === "codex-canvas"))
    throw new Error("The backend entry point is missing.");
  const digest = createHash("sha256");
  for (const [name, file] of sources) {
    const content = createHash("sha256")
      .update(fs.readFileSync(file))
      .digest("hex");
    digest.update(`${name}\0${content}\n`);
  }
  return digest.digest("hex");
}
function updateStatus(resources, running) {
  let availableBackendBuild;
  try {
    availableBackendBuild = backendBuild(resources);
  } catch {
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
  try {
    const saved = JSON.parse(
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
      ...updateStatus(resources, existing),
      origin,
      owned: false,
    };
  const script = path.join(resources, "scripts/codex-canvas");
  if (
    !fs.existsSync(script) ||
    !fs.existsSync(path.join(resources, "web/dist/index.html"))
  )
    throw new Error(
      "Desktop assets are missing. Run the web build before starting or packaging desktop.",
    );
  const python = executable("python3", env);
  execFileSync(
    python,
    [
      "-c",
      'import sys; assert sys.version_info >= (3,11), "Python 3.11 or later is required"',
    ],
    { timeout: 5000 },
  );
  const codex = executable("codex", env);
  if (supervisorMode) {
    const supervisor = path.join(
      resources,
      "scripts/codex_process_supervisor.py",
    );
    if (!fs.existsSync(supervisor))
      throw new Error(
        "Supervisor mode is enabled but its packaged script is missing.",
      );
    const log = path.join(canonicalState, "supervisor.log");
    const fd = fs.openSync(log, "a", 0o600);
    let started = false;
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
            env: { ...env, CODEX_AGENTS_STATE_DIR: canonicalState },
          },
        );
        lastError = null;
        break;
      } catch (error) {
        lastError = error;
        if (!started) {
          started = true;
          let child;
          try {
            // The supervisor's state-directory lease elects one owner when the
            // desktop and LaunchAgent both start it during the same recovery.
            child = spawn(
              python,
              ["-B", supervisor, "--state", canonicalState],
              {
                cwd: os.homedir(),
                detached: true,
                stdio: ["ignore", fd, fd],
                env: {
                  ...env,
                  CODEX_AGENTS_STATE_DIR: canonicalState,
                  CODEX_AGENTS_SUPERVISOR_MODE: "1",
                },
              },
            );
            child.on("error", () => {});
            child.unref();
          } catch (spawnError) {
            lastError = spawnError;
          }
        }
        await sleep(100);
      }
    }
    fs.closeSync(fd);
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
        ...env,
        CODEX_AGENTS_STATE_DIR: canonicalState,
        CODEX_AGENTS_SUPERVISOR_MODE: supervisorMode ? "1" : "0",
        CODEX_BIN: codex,
        CODEX_NODE: process.execPath,
        PATH: `${path.dirname(codex)}:${env.PATH || "/usr/bin:/bin"}`,
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
        ...updateStatus(resources, ready),
        origin,
        owned: ready.pid === child.pid,
      };
    }
    if (child.exitCode !== null)
      throw new Error(
        `Backend exited (${child.exitCode}). Read ${log}. Another runtime may own this state directory.`,
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
};
