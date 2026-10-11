import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

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

try {
  const result = spawnSync("pnpm", ["exec", "playwright", "test", ...args], {
    stdio: "inherit",
    env: environment,
  });

  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  rmSync(isolatedHome, { recursive: true, force: true });
  for (const directory of Object.values(xdgDirectories))
    rmSync(directory, { recursive: true, force: true });
  console.log(`Playwright output: ${outputDirectory}`);
}
