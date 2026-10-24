#!/usr/bin/env node
// Native full login only. This helper never reads or prints credential values.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";
import {
  defaultKeychainConfig,
  keychainConfig,
  nativeLoginSpec,
  prepareKeychainConfig,
} from "./claude-auth.mjs";
import { profiles, startProxy } from "./canary.mjs";

/** OAuth needs an ephemeral local callback, not direct outbound network access. */
export function loginProfile(args) {
  return (
    profiles({ ...args, claudeKeychain: true }).supervisor +
    `
(allow network-bind (local tcp "localhost:*"))
(allow network-inbound (local tcp "localhost:*"))
`
  );
}

/** Stop only this native child after cancellation. Never repeat an uncertain login. */
export function runNativeLogin(spec, spawnProcess = spawn, signals = process) {
  return new Promise((resolve, reject) => {
    const child = spawnProcess(spec.file, spec.args, {
      cwd: spec.cwd,
      env: spec.env,
      stdio: "inherit",
    });
    let cancelled = false;
    let timer;
    const stop = (signal) => {
      cancelled = true;
      child.kill(signal);
      // Milliseconds allowed for native login to exit after cancellation.
      timer ??= setTimeout(() => child.kill("SIGKILL"), 5_000).unref();
    };
    const interrupt = () => stop("SIGINT");
    const terminate = () => stop("SIGTERM");
    const cleanup = () => {
      clearTimeout(timer);
      signals.removeListener("SIGINT", interrupt);
      signals.removeListener("SIGTERM", terminate);
    };
    signals.on("SIGINT", interrupt);
    signals.on("SIGTERM", terminate);
    child.once("error", (error) => {
      cleanup();
      reject(error);
    });
    child.once("exit", (status) => {
      cleanup();
      resolve(cancelled ? 1 : (status ?? 1));
    });
  });
}

export async function login(
  { config, executable, liveState },
  spawnProcess = spawn,
) {
  if (process.platform !== "darwin")
    throw new Error("Native canary Keychain login requires macOS.");
  keychainConfig(config);
  const tmp = fs.realpathSync(os.tmpdir());
  const root = fs.mkdtempSync(path.join(tmp, "studio-canary-login-"));
  fs.chmodSync(root, 0o700);
  const proxy = await startProxy();
  try {
    const profile = path.join(root, "login.sb");
    fs.writeFileSync(
      profile,
      loginProfile({
        root,
        tmp,
        home: os.homedir(),
        liveState,
        app: "/Applications/Codex Studio.app",
        port: null,
        proxyPort: proxy.port,
        readable: [],
        writable: [config],
      }),
      { mode: 0o600 },
    );
    const spec = nativeLoginSpec({
      config,
      tmp,
      profile,
      executable,
      proxyURL: `http://127.0.0.1:${proxy.port}`,
    });
    // Full auth login prints its authorization URL and result, never setup-token output.
    const code = await runNativeLogin(spec, spawnProcess);
    if (code !== 0)
      throw new Error(
        "Native canary Claude login failed. The private profile remains available for review.",
      );
    keychainConfig(config);
    process.stdout.write(
      "Native login finished. The canary still requires native account and subscription proof.\n",
    );
    return 0;
  } finally {
    proxy.server.closeAllConnections();
    await new Promise((resolve) => proxy.server.close(resolve));
    // Only this helper's profile lives here. Native auth stays in its private config and Keychain entry.
    fs.rmSync(root, { recursive: true, force: true });
  }
}

async function cli() {
  const { values } = parseArgs({
    options: {
      config: { type: "string", default: defaultKeychainConfig() },
      claude: { type: "string" },
      "live-state": {
        type: "string",
        default: path.join(os.homedir(), ".local/state/codex-agents"),
      },
      prepare: { type: "boolean", default: false },
      login: { type: "boolean", default: false },
    },
  });
  const config = path.resolve(values.config);
  if (values.prepare === values.login)
    throw new Error("Select exactly one of --prepare and --login.");
  if (values.prepare) {
    prepareKeychainConfig(config);
    process.stdout.write(
      `Private native canary profile prepared: ${config}\nNo login started.\n`,
    );
    return 0;
  }
  if (!values.claude)
    throw new Error(
      "Set --claude to the absolute native Claude executable path.",
    );
  return login({
    config,
    executable: values.claude,
    liveState: path.resolve(values["live-state"]),
  });
}

if (
  process.argv[1] &&
  fileURLToPath(import.meta.url) === fs.realpathSync(process.argv[1])
)
  cli().then(
    (code) => {
      process.exitCode = code;
    },
    () => {
      // Do not render arbitrary native errors or captured output at this boundary.
      process.stderr.write(
        "Canary Claude login stopped. Use a private prepared profile and an absolute native executable path.\n",
      );
      process.exitCode = 1;
    },
  );
