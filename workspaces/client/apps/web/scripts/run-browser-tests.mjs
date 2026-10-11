import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

function defaultBrowserPath() {
  if (process.platform === "win32")
    return join(
      process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"),
      "ms-playwright",
    );
  if (process.platform === "darwin")
    return join(homedir(), "Library", "Caches", "ms-playwright");
  return join(homedir(), ".cache", "ms-playwright");
}

const args = process.argv.slice(2);
const separatorIndex = args.indexOf("--");
if (separatorIndex !== -1) args.splice(separatorIndex, 1);

const environment = { ...process.env };
environment.PLAYWRIGHT_BROWSERS_PATH ??= defaultBrowserPath();
const stateRoot = mkdtempSync(
  join(
    process.platform === "win32" ? tmpdir() : "/var/tmp",
    "codex-playwright-",
  ),
);
const outputDirectory = join(stateRoot, "playwright-output");
mkdirSync(outputDirectory);
environment.PLAYWRIGHT_OUTPUT_DIR = outputDirectory;
const isolatedHome = join(stateRoot, "home");
const xdgDirectories = {
  XDG_CONFIG_HOME: join(stateRoot, "config"),
  XDG_CACHE_HOME: join(stateRoot, "cache"),
  XDG_DATA_HOME: join(stateRoot, "data"),
  XDG_STATE_HOME: join(stateRoot, "state"),
  XDG_RUNTIME_DIR: join(stateRoot, "runtime"),
};
mkdirSync(isolatedHome);
for (const [key, directory] of Object.entries(xdgDirectories)) {
  mkdirSync(directory);
  environment[key] = directory;
}
environment.HOME = isolatedHome;
environment.CODEX_HOME = join(isolatedHome, ".codex");
environment.CLAUDE_CONFIG_DIR = join(isolatedHome, ".claude");

const repositoryRoot = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);
const pythonResolver = join(
  repositoryRoot,
  "workspaces/runtime/apps/server/src/codex_python.py",
);
const pythonBootstrap =
  environment.PYTHON_BIN || environment.PYTHON || "python3";
const pythonResolution = environment.CODEX_AGENTS_PYTHON
  ? { status: 0, stdout: environment.CODEX_AGENTS_PYTHON }
  : spawnSync(pythonBootstrap, [pythonResolver], {
      encoding: "utf8",
      env: environment,
    });
const pythonPath = pythonResolution.stdout?.trim();
const serverSourceDirectory = join(
  repositoryRoot,
  "workspaces/runtime/apps/server/src",
);
const pythonEnvironment = {
  ...environment,
  PYTHONPATH: [serverSourceDirectory, environment.PYTHONPATH]
    .filter(Boolean)
    .join(delimiter),
  PYTHONDONTWRITEBYTECODE: "1",
};
const pythonProbe = pythonPath
  ? spawnSync(
      pythonPath,
      [
        "-c",
        "import fastapi, httpx, pydantic, uvicorn, watchdog, codex_operations_adapter",
      ],
      { encoding: "utf8", env: pythonEnvironment },
    )
  : null;
const pythonReady =
  pythonResolution.status === 0 && pythonPath && pythonProbe?.status === 0;

try {
  if (!pythonReady) {
    const diagnostic =
      pythonProbe?.stderr?.trim() ||
      pythonResolution.stderr?.trim() ||
      `resolved ${pythonPath || "no Python interpreter"} with protocol ${pythonProbe?.stdout?.trim() || "unavailable"}`;
    console.error(
      `Browser tests require a prepared Python environment with the native operations module that matches this checkout. ${diagnostic}\nPrepare it with \`cargo build --release -p studio-operations-python\` and \`python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir <test-bin>\`, then set CODEX_AGENTS_PYTHON to that environment's Python executable.`,
    );
    process.exitCode = 1;
  } else {
    environment.CODEX_AGENTS_PYTHON = pythonPath;
    environment.PYTHON_BIN = pythonPath;
    environment.PYTHON = pythonPath;
    const pythonDirectory = dirname(pythonPath);
    const pathEntries = (environment.PATH || "").split(delimiter);
    if (pathEntries[0] !== pythonDirectory)
      environment.PATH = [pythonDirectory, ...pathEntries].join(delimiter);
    console.log(`Playwright fixture Python ${pythonPath}`);
    const result = spawnSync("pnpm", ["exec", "playwright", "test", ...args], {
      stdio: "inherit",
      env: environment,
    });

    if (result.error) throw result.error;
    process.exitCode = result.status ?? 1;
  }
} finally {
  rmSync(isolatedHome, { recursive: true, force: true });
  for (const directory of Object.values(xdgDirectories))
    rmSync(directory, { recursive: true, force: true });
  console.log(`Playwright output: ${outputDirectory}`);
}
