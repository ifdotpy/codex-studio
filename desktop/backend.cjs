const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { createHash } = require("node:crypto");
const { spawn, execFileSync } = require("node:child_process");
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function backendBuild(resources) {
  const scripts = path.join(resources, "scripts");
  const names = fs
    .readdirSync(scripts)
    .filter(
      (name) =>
        (name.endsWith(".py") || name === "codex-canvas") &&
        fs.statSync(path.join(scripts, name)).isFile(),
    )
    .sort();
  if (!names.includes("codex-canvas"))
    throw new Error("The backend entry point is missing.");
  const digest = createHash("sha256");
  for (const name of names) {
    const content = createHash("sha256")
      .update(fs.readFileSync(path.join(scripts, name)))
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
    // A legacy backend has no source identity. An app restart cannot prove
    // that the code in the running process matches the installed files.
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
  const origin = `http://127.0.0.1:${port}`;
  const existing = await identity(origin, canonicalState);
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
        CODEX_BIN: codex,
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
