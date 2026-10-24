#!/usr/bin/env node
// Upgrade canary: install build N+1 over build N on an isolated clone, with real
// processes, before the same install runs on the owner's Mac.
//
// It starts build N (a copy of the app) on a new small state with its own state
// folder, port and launchd stand-in, with the real process supervisor, recovery
// job and provider CLIs. Its chats run on the owner's logins in new throwaway
// sessions only, on the cheapest model. Then it installs build N+1 the way a
// paired-server install does (staged copy, the real installer of build N, the
// recovery job restarted from the new app) and checks that the provider handles,
// chats, steer and queued message survive. docs/testing.md, "Upgrade canary",
// owns the contract.
//
// Isolation: every job runs in a sandbox that writes only below the run folder
// and the user temporary folder, cannot reach the live state
// sockets, /Applications or the live app, and can reach the network only
// through the canary's proxy. The proxy refuses auth.openai.com, so a clone
// never refreshes (and never rotates) a live Codex login; the Codex homes only
// link the live auth.json and config.toml, which the sandbox keeps read-only.
// The Claude login is the canary's own (CANARY_CLAUDE_CONFIG_DIR), never a live one.
// Keychain stays denied unless --claude-keychain or CANARY_CLAUDE_KEYCHAIN=1 is explicit.

import { createHash, randomBytes, randomUUID } from "node:crypto";
import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import {
  claudeAccount,
  claudeConfigEnvironment,
  fileConfig,
} from "./claude-auth.mjs";
import { studioProfile } from "./studio-claude-profile.mjs";
import {
  CheckFailed,
  Report,
  cheapestModel,
  lowestEffort,
  messageItems,
} from "../installed-smoke/smoke.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_FROM = "/Applications/Codex Studio.app";
const APP_NAME = "Codex Studio.app";
/** Prefix of the run folder below the platform temporary directory. */
export const RUN_PREFIX = "studio-canary-";
/** Marker file in the run folder; it holds the run id. */
export const RUN_MARKER = "studio-canary-run.json";
/** Hosts the canary proxy forwards. Every other host is refused and reported. */
export const PROXY_ALLOWED_HOSTS = [
  "chatgpt.com",
  "ab.chatgpt.com",
  "api.openai.com",
  "api.anthropic.com",
  "statsig.anthropic.com",
  "console.anthropic.com",
  "platform.claude.com",
  "claude.ai",
  "models.dev",
  "oaiusercontent.com",
];
/** Token endpoint of the live Codex logins: a refresh from the clone would rotate them. */
export const PROXY_REFUSED_HOSTS = ["auth.openai.com"];
/** Log lines that mean a provider start or reattach went wrong. */
export const FAILURE_LINES = /did not start|could not be verified/i;
/** Milliseconds for one HTTP request to the clone backend. */
const REQUEST_MS = 30_000;
/** Milliseconds for a backend to answer after its job starts. */
const BACKEND_MS = 180_000;
/** Milliseconds for a chat turn to reach a state. */
const TURN_MS = 300_000;
/** Milliseconds for the installer of build N. */
const INSTALL_MS = 900_000;
/** Milliseconds launchd waits before it starts an exited KeepAlive job again (ThrottleInterval 10). */
const KEEPALIVE_THROTTLE_MS = 10_000;
/** Milliseconds between two state polls. */
const POLL_MS = 1_000;
/** Seconds of the monitor command: long enough to run across the install. */
const MONITOR_SECONDS = 150;
/** Seconds of the shell command of the steered turn: it runs while the steer arrives. */
const STEER_SECONDS = 45;
/** Pricing owner: the models.dev artifact that the backend keeps in its state folder. */
const PRICING_ARTIFACT = "model-pricing/models-dev-v1.json";
/** The launchd PATH that the recovery helper and the jobs see. */
const LAUNCHD_PATH = "/usr/bin:/bin:/usr/sbin:/sbin";
const TERMINAL = new Set([
  "completed",
  "idle",
  "failed",
  "interrupted",
  "blocked",
  "paused",
]);
const FAILED = new Set(["failed", "interrupted", "blocked"]);

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function until(label, ms, probe) {
  const deadline = Date.now() + ms;
  let note = "";
  for (;;) {
    const result = await probe();
    if (result.done) return result.value;
    note = result.note ?? note;
    if (Date.now() >= deadline)
      throw new CheckFailed(
        `${label}: not reached in ${ms / 1000} s (${note})`,
      );
    await sleep(POLL_MS);
  }
}

function sha256File(file) {
  return createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function readJSON(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

/** A build stamp of an app bundle. */
export function buildStamp(app) {
  return readJSON(path.join(app, "Contents/Resources/studio-build.json"));
}

/** Rows of a read-only SQLite query, as arrays of strings. */
function query(database, sql) {
  if (!fs.existsSync(database)) return [];
  const out = execFileSync(
    "/usr/bin/sqlite3",
    ["-separator", "\t", `file:${database}?mode=ro`, sql],
    {
      encoding: "utf8",
    },
  );
  return out
    .split("\n")
    .filter(Boolean)
    .map((line) => line.split("\t"));
}

/** Open supervisor handles as "id|pid|generation" lines, sorted. */
export function openHandles(state) {
  return query(
    path.join(state, "supervisor.sqlite3"),
    "select id, pid, generation from handles where closed_at is null order by id",
  ).map((row) => row.join("|"));
}

/** Handles that changed: lines that are only before or only after. */
export function handleChanges(before, after) {
  const was = new Set(before);
  const now = new Set(after);
  return [
    ...before.filter((line) => !now.has(line)).map((line) => `- ${line}`),
    ...after.filter((line) => !was.has(line)).map((line) => `+ ${line}`),
  ];
}

/** The decision of the canary proxy for one host. */
export function proxyDecision(host) {
  const name = String(host).toLowerCase().replace(/\.$/, "");
  if (PROXY_REFUSED_HOSTS.includes(name)) return "refused";
  return PROXY_ALLOWED_HOSTS.some(
    (allowed) => name === allowed || name.endsWith(`.${allowed}`),
  )
    ? "allowed"
    : "refused";
}

/** A CONNECT proxy that forwards only the allowed hosts and counts every request. */
function startProxy() {
  const seen = new Map();
  const server = http.createServer((request, response) => {
    // Plain HTTP is never needed by the providers; refuse and count it.
    const host = (() => {
      try {
        return new URL(request.url).hostname;
      } catch {
        return "invalid";
      }
    })();
    const key = `${host} http refused`;
    seen.set(key, (seen.get(key) ?? 0) + 1);
    response.writeHead(403).end();
  });
  server.on("connect", (request, socket, head) => {
    const [host, portText] = String(request.url).split(":");
    const port = Number(portText) || 443;
    const decision = proxyDecision(host);
    const key = `${host} ${decision}`;
    seen.set(key, (seen.get(key) ?? 0) + 1);
    if (decision !== "allowed" || port !== 443) {
      socket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
      return;
    }
    const upstream = net.connect(port, host, () => {
      socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head?.length) upstream.write(head);
      upstream.pipe(socket);
      socket.pipe(upstream);
    });
    upstream.on("error", () => socket.destroy());
    socket.on("error", () => upstream.destroy());
  });
  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port, seen }),
    ),
  );
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

/** Sandbox profiles of the canary jobs. Later rules win over earlier allows. */
export function profiles({
  root,
  tmp,
  home,
  liveState,
  app,
  port,
  proxyPort,
  readable,
  writable = [],
  claudeKeychain = false,
  claudeProfile = null,
}) {
  const quote = (value) => JSON.stringify(value);
  const common = `(version 1)
(deny default)
(allow file-read*)
(allow file-ioctl)
(allow process-fork)
(allow signal (target self) (target children) (target same-sandbox))
(allow sysctl-read)
(allow process-info*)
(allow mach-lookup)
(allow ipc-posix-shm)
(allow system-socket)
(allow pseudo-tty)
(allow file-write* (subpath ${quote(root)}) (subpath ${quote(tmp)})${writable.map((folder) => ` (subpath ${quote(folder)})`).join("")} (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper"))
`;
  const keychainDeny = `(deny mach-lookup (global-name "com.apple.SecurityServer") (global-name "com.apple.securityd") (global-name "com.apple.securityd.xpc") (global-name "com.apple.security.agent") (global-name "com.apple.security.authhost"))`;
  const auth = claudeKeychain
    ? common.trimEnd()
    : `${common.trimEnd()}\n${keychainDeny}`;
  const live = `(deny file-write* (subpath ${quote(liveState)}) (subpath "/Applications") (subpath ${quote(path.join(home, ".codex"))}) (subpath ${quote(path.join(home, ".claude"))}) (subpath ${quote(path.join(home, "Library/LaunchAgents"))}))
${readable.map((file) => `(deny file-write* (literal ${quote(file)}))`).join("\n")}
(deny network-outbound (remote unix-socket (subpath ${quote(liveState)})))
(deny process-exec (subpath "/Applications/Codex Studio.app") (literal ${quote(path.join(app, "Contents/Resources/studio-linux-vm"))}))
(deny file-read* (subpath ${quote(path.join(home, ".ssh"))}) (subpath ${quote(path.join(home, ".config/gh"))}) (literal ${quote(path.join(home, ".gitconfig"))}))
;; Last, so that it wins: /bin/ps is setuid, and a sandbox cannot run a setuid program.
;; Recovery and the installer read process start times with it; it only lists processes.
(allow process-exec (literal "/bin/ps") (with no-sandbox))`;
  // Native login has no clone backend. Its only outbound TCP destination is the proxy.
  const backend = (direction) =>
    port === null ? "" : ` (${direction} tcp ${quote(`localhost:${port}`)})`;
  const loopback = `(allow network-outbound (remote tcp ${quote(`localhost:${proxyPort}`)})${backend("remote")} (remote unix-socket (subpath ${quote(root)})))
(allow network-bind${backend("local")} (local unix-socket (subpath ${quote(root)})))
(allow network-inbound${backend("local")} (local unix-socket (subpath ${quote(root)})))`;
  // A fresh Studio Add profile can live inside liveState. Only its proven exact
  // directory is writable; the live sockets and every sibling remain denied.
  const dedicated =
    claudeKeychain && claudeProfile
      ? `\n(allow file-write* (subpath ${quote(claudeProfile)}))`
      : "";
  return {
    // The supervisor and its providers: real CLIs, network only through the proxy.
    supervisor: `${auth}
(allow process-exec)
${loopback}
${live}
(deny process-exec (literal "/usr/bin/ssh") (literal "/usr/bin/scp") (literal "/usr/bin/git"))${dedicated}`,
    // The recovery job and the server.
    recovery: `${auth}
(allow process-exec)
${loopback}
${live}
(deny process-exec (literal "/usr/bin/osascript") (literal "/usr/bin/open"))${dedicated}`,
    // The installer of build N: no window tools, writes only below the run folder.
    installer: `${common.trimEnd()}\n${keychainDeny}
(allow process-exec)
(allow signal)
${loopback}
${live}
(deny process-exec (literal "/usr/bin/osascript") (literal "/usr/bin/open"))`,
  };
}

/** The live side that the canary must never change. */
function liveFingerprint(liveState) {
  const files = [
    path.join(liveState, "background-recovery.json"),
    path.join(
      "/Applications",
      APP_NAME,
      "Contents/Resources/studio-build.json",
    ),
  ];
  const plists = fs
    .readdirSync(path.join(os.homedir(), "Library/LaunchAgents"))
    .filter(
      (name) =>
        name.startsWith("local.codex.agents.") && name.endsWith(".plist"),
    )
    .sort()
    .map((name) => path.join(os.homedir(), "Library/LaunchAgents", name));
  return Object.fromEntries(
    [...files, ...plists].map((file) => [
      file,
      fs.existsSync(file) ? sha256File(file) : null,
    ]),
  );
}

class Backend {
  constructor(port) {
    this.url = `http://127.0.0.1:${port}`;
    this.token = "";
  }

  async request(method, route, body, ms = REQUEST_MS) {
    const headers = { accept: "application/json" };
    if (body !== undefined) headers["content-type"] = "application/json";
    if (method !== "GET") headers["x-canvas-token"] = this.token;
    const response = await fetch(this.url + route, {
      method,
      headers,
      redirect: "manual",
      signal: AbortSignal.timeout(ms),
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    if (!response.ok)
      throw new CheckFailed(
        `${method} ${route}: HTTP ${response.status} ${text.slice(0, 300)}`,
      );
    return text ? JSON.parse(text) : null;
  }

  get(route, ms) {
    return this.request("GET", route, undefined, ms);
  }

  post(route, body, ms) {
    return this.request("POST", route, body, ms);
  }

  async identity() {
    try {
      return await this.get("/api/desktop", 5_000);
    } catch {
      return null;
    }
  }
}

/** One launchd job of the clone, started in its sandbox; KeepAlive restarts it after 10 s. */
class Job {
  constructor({ name, argv, env, profile, log }) {
    Object.assign(this, { name, argv, env, profile, log });
    this.child = null;
    this.keepAlive = true;
    this.starts = 0;
  }

  start() {
    const output = fs.openSync(this.log, "a");
    this.child = spawn(
      "/usr/bin/sandbox-exec",
      ["-f", this.profile, ...this.argv],
      {
        cwd: "/",
        env: this.env,
        stdio: ["ignore", output, output],
        detached: true,
      },
    );
    fs.closeSync(output);
    this.starts += 1;
    this.child.on("exit", () => {
      this.child = null;
      if (this.keepAlive)
        setTimeout(
          () => this.keepAlive && !this.child && this.start(),
          KEEPALIVE_THROTTLE_MS,
        );
    });
  }

  async stop() {
    this.keepAlive = false;
    const child = this.child;
    if (!child) return;
    const exited = new Promise((resolve) => child.once("exit", resolve));
    child.kill("SIGTERM");
    await Promise.race([exited, sleep(30_000)]);
    if (this.child)
      throw new CheckFailed(`${this.name} job did not stop on SIGTERM`);
  }
}

export class Canary {
  constructor(options) {
    this.from = path.resolve(options.from);
    this.to = path.resolve(options.to);
    this.codexAccount = options["codex-account"];
    this.claudeConfig =
      options["claude-config"] && !options["no-claude"]
        ? path.resolve(options["claude-config"])
        : null;
    this.claudeKeychain = options["claude-keychain"] === true;
    this.claudeReservation = options["claude-reservation"]
      ? path.resolve(options["claude-reservation"])
      : null;
    this.claudeProfileHome = os.homedir();
    if (
      this.claudeKeychain !== Boolean(this.claudeReservation) ||
      (this.claudeReservation && options["no-claude"])
    )
      throw new CheckFailed(
        "Keychain mode requires a bound fresh Studio Add reservation.",
      );
    this.python = options.python;
    this.node = options.node;
    this.keep = options.keep;
    this.liveState = path.resolve(options["live-state"]);
    this.report = new Report();
    this.runId = randomBytes(4).toString("hex");
    this.temporaryRoot = fs.realpathSync(os.tmpdir());
    this.jobs = {};
    this.chats = {};
    this.accounts = [];
  }

  get app() {
    return path.join(this.root, "apps", APP_NAME);
  }

  get state() {
    return path.join(this.root, "state");
  }

  token(name) {
    return `CANARY_${this.runId}_${name}`;
  }

  // --- preparation -------------------------------------------------------------

  prepare() {
    const from = buildStamp(this.from);
    const to = buildStamp(this.to);
    if (!(to.builtAt > from.builtAt))
      throw new CheckFailed(
        `build N+1 ${to.commit.slice(0, 9)} is not newer than build N ${from.commit.slice(0, 9)}`,
      );
    this.root = fs.mkdtempSync(path.join(this.temporaryRoot, RUN_PREFIX));
    fs.writeFileSync(
      path.join(this.root, RUN_MARKER),
      JSON.stringify({ runId: this.runId, pid: process.pid }),
    );
    for (const folder of [
      "apps",
      "state",
      "home",
      "homes",
      "work",
      "logs",
      "profiles",
    ])
      // The state folder is private, as the app creates it.
      fs.mkdirSync(path.join(this.root, folder), {
        recursive: true,
        mode: 0o700,
      });
    execFileSync("/bin/cp", ["-c", "-R", this.from, this.app]);
    this.fingerprint = liveFingerprint(this.liveState);
    this.builds = { from, to };
    return `N ${from.commit.slice(0, 9)} -> N+1 ${to.commit.slice(0, 9)} in ${this.root}`;
  }

  /** The clone registry: the chosen Codex login (linked read-only) and the canary Claude login. */
  accountsFile() {
    const dedicated = this.claudeReservation
      ? studioProfile(this.claudeReservation, this.liveState, {
          storage: this.claudeOwnershipStorage,
        })
      : null;
    if (dedicated) {
      if (this.claudeConfig && this.claudeConfig !== dedicated.config)
        throw new CheckFailed(
          "The config differs from the bound Studio canary profile.",
        );
      this.claudeConfig = dedicated.config;
    } else if (this.claudeConfig)
      fileConfig(this.claudeConfig, { home: this.claudeProfileHome });
    const live = readJSON(path.join(this.liveState, "accounts/registry.json"));
    const codex = live.accounts?.[this.codexAccount];
    if (!codex || codex.provider === "claude")
      throw new CheckFailed(
        `no Codex account ${this.codexAccount} in the live registry`,
      );
    const home = path.join(this.root, "homes", "codex");
    fs.mkdirSync(home);
    this.readable = [];
    for (const name of ["auth.json", "config.toml"]) {
      const target = path.join(codex.home, name);
      if (!fs.existsSync(target)) continue;
      fs.symlinkSync(target, path.join(home, name));
      this.readable.push(fs.realpathSync(target));
    }
    if (!fs.existsSync(path.join(home, "auth.json")))
      throw new CheckFailed(
        `the Codex login ${this.codexAccount} has no auth.json (file credentials)`,
      );
    const accounts = { [this.codexAccount]: { ...codex, home } };
    this.accounts = [{ key: this.codexAccount, provider: "codex" }];
    const credentials =
      this.claudeConfig && path.join(this.claudeConfig, ".credentials.json");
    if (credentials && !this.claudeKeychain && !fs.existsSync(credentials)) {
      // The Claude part fails alone; the Codex part still runs.
      this.claudeProblem = `the canary Claude login ${credentials} is missing; see docs/testing.md, "Upgrade canary"`;
    } else if (credentials) {
      const profile = dedicated
        ? { emailAddress: dedicated.account.email }
        : claudeAccount(this.claudeConfig);
      accounts["claude-canary"] = {
        id: "claude-canary",
        provider: "claude",
        label: "Canary Claude",
        source: "Claude Code",
        status: "ready",
        email: profile.emailAddress,
        accountId: `claude:${profile.emailAddress}`,
        _credentialIdentity:
          dedicated?.account._credentialIdentity ??
          `claude:${profile.emailAddress}`,
        home: this.claudeConfig,
        claudeOptions: {
          configDir: this.claudeConfig,
          ...(dedicated ? { binaryPath: dedicated.binary } : {}),
        },
      };
      this.accounts.push({ key: "claude-canary", provider: "claude" });
    }
    // A fresh Rust state accepts only a private accounts folder with a private registry.
    fs.mkdirSync(path.join(this.state, "accounts"), {
      recursive: true,
      mode: 0o700,
    });
    fs.writeFileSync(
      path.join(this.state, "accounts/registry.json"),
      JSON.stringify(
        {
          version: live.version ?? 1,
          defaultAccountKey: this.codexAccount,
          accounts,
          logins: {},
        },
        null,
        2,
      ),
      { mode: 0o600 },
    );
    return this.accounts
      .map((account) => `${account.provider}: ${account.key}`)
      .join(", ");
  }

  /** Step 10 of a switch on the clone: build N's recovery helper with a launchd stand-in. */
  async configure() {
    const loaded = {};
    const plistArguments = (file) =>
      [
        ...fs
          .readFileSync(file, "utf8")
          .match(/<key>ProgramArguments<\/key><array>([\s\S]*?)<\/array>/)[1]
          .matchAll(/<string>([^<]*)<\/string>/g),
      ].map((match) =>
        match[1]
          .replaceAll("&amp;", "&")
          .replaceAll("&lt;", "<")
          .replaceAll("&gt;", ">")
          .replaceAll("&quot;", '"')
          .replaceAll("&apos;", "'"),
      );
    const label = (file) =>
      fs
        .readFileSync(file, "utf8")
        .match(/<key>Label<\/key><string>([^<]+)<\/string>/)[1];
    const launchAgents = path.join(
      this.root,
      "home",
      "Library",
      "LaunchAgents",
    );
    const run = async (file, args) => {
      if (file !== "/bin/launchctl")
        throw new CheckFailed(`the canary refuses to run ${file}`);
      const [verb, target, plist] = args;
      if (verb === "print") {
        if (loaded[target]) return { stdout: "state = running\n" };
        throw Object.assign(new Error(`Could not find service "${target}"`), {
          code: 113,
        });
      }
      if (verb === "bootstrap") {
        if (!path.resolve(plist).startsWith(launchAgents + path.sep))
          throw new CheckFailed(`refusing plist ${plist}`);
        loaded[`${target}/${label(plist)}`] = plistArguments(plist);
        return { stdout: "" };
      }
      if (verb === "bootout") {
        delete loaded[target];
        return { stdout: "" };
      }
      throw new CheckFailed(`the canary refuses launchctl ${verb}`);
    };
    const helpers = path.join(
      this.app,
      "Contents/Resources/helpers/desktop-recovery/recovery.cjs",
    );
    const { configureRecovery } = createRequire(import.meta.url)(helpers);
    const result = await configureRecovery({
      applicationPath: `/Applications/${APP_NAME}/Contents/MacOS/studio-native-client`,
      resources: path.join(this.app, "Contents/Resources/workspace"),
      supervisor: path.join(this.app, "Contents/Resources/recover_backend.py"),
      port: this.port,
      enabled: true,
      restartEnvironment: { CODEX_AGENTS_SUPERVISOR_MODE: "1" },
      env: {
        HOME: os.homedir(),
        PATH: `${path.dirname(this.python)}:${LAUNCHD_PATH}`,
        CODEX_AGENTS_STATE_DIR: this.state,
      },
      home: path.join(this.root, "home"),
      run,
    });
    const uid = process.getuid();
    this.jobArguments = {
      supervisor: loaded[`gui/${uid}/${result.supervisorLabel}`],
      recovery: loaded[`gui/${uid}/${result.label}`],
    };
    for (const [name, argv] of Object.entries(this.jobArguments)) {
      if (!argv)
        throw new CheckFailed(`the recovery helper registered no ${name} job`);
      for (const part of argv)
        if (
          part.startsWith("/Applications/") ||
          part.startsWith(this.liveState)
        )
          throw new CheckFailed(
            `the ${name} job names the live app or state: ${part}`,
          );
    }
    return `port ${this.port}; recovery runs ${this.jobArguments.recovery[0]}`;
  }

  /** Native startup discovery must never fall back to the owner's live profile. */
  claudeEnvironment() {
    return claudeConfigEnvironment(
      this.claudeConfig ?? path.join(this.root, "homes", "claude-disabled"),
    );
  }

  /** The proxy, the clone port, the sandbox profiles and the launchd environment of the jobs. */
  async sandbox() {
    if (
      this.claudeReservation &&
      studioProfile(this.claudeReservation, this.liveState, {
        storage: this.claudeOwnershipStorage,
      }).config !== this.claudeConfig
    )
      throw new CheckFailed(
        "The Studio canary profile changed before sandbox admission.",
      );
    const proxy = await startProxy();
    this.proxy = proxy;
    this.port = await freePort();
    const tmp = fs.realpathSync(
      execFileSync("/usr/bin/getconf", ["DARWIN_USER_TEMP_DIR"], {
        encoding: "utf8",
      }).trim(),
    );
    const text = profiles({
      root: fs.realpathSync(this.root),
      tmp,
      home: os.homedir(),
      liveState: this.liveState,
      app: this.app,
      port: this.port,
      proxyPort: proxy.port,
      readable: this.readable,
      claudeKeychain: this.claudeKeychain,
      claudeProfile: this.claudeKeychain ? this.claudeConfig : null,
      // The canary's own Claude login: the CLI keeps its sessions and refreshed login there.
      writable: this.accounts.some((account) => account.provider === "claude")
        ? [fs.realpathSync(this.claudeConfig)]
        : [],
    });
    this.profiles = {};
    for (const [name, body] of Object.entries(text)) {
      this.profiles[name] = path.join(this.root, "profiles", `${name}.sb`);
      fs.writeFileSync(this.profiles[name], body);
    }
    const proxyURL = `http://127.0.0.1:${proxy.port}`;
    // The same environment for build N and N+1: a changed launch environment is what the canary detects.
    this.env = {
      HOME: os.homedir(),
      USER: os.userInfo().username,
      LOGNAME: os.userInfo().username,
      SHELL: "/bin/zsh",
      TMPDIR: `${tmp}/`,
      PATH: LAUNCHD_PATH,
      SSL_CERT_FILE: "/etc/ssl/cert.pem",
      HTTPS_PROXY: proxyURL,
      HTTP_PROXY: proxyURL,
      ALL_PROXY: proxyURL,
      https_proxy: proxyURL,
      http_proxy: proxyURL,
      NO_PROXY: "localhost,127.0.0.1",
      no_proxy: "localhost,127.0.0.1",
      ...this.claudeEnvironment(),
      // The image workspace store defaults to the live state's folder whatever the state
      // folder is (image_workspace.rs); the clone keeps its own.
      CODEX_WORKSPACE_STORE: path.join(
        fs.realpathSync(this.state),
        "workspaces",
      ),
    };
    this.backend = new Backend(this.port);
    return `proxy ${proxy.port}, clone port ${this.port}`;
  }

  /**
   * The first app start creates the state, as the native app does on a new Mac: a fresh
   * Rust state refuses entries that the recovery job makes before the server (its lock).
   */
  async initializeState() {
    const server = path.join(
      this.app,
      "Contents/Resources/workspace/bin/studio-runtime-server",
    );
    const job = new Job({
      name: "first start",
      argv: [server, "--port", String(this.port)],
      env: {
        ...this.env,
        CODEX_AGENTS_STATE_DIR: this.state,
        CODEX_AGENTS_SUPERVISOR_MODE: "0",
      },
      profile: this.profiles.recovery,
      log: path.join(this.root, "logs", "first-start.log"),
    });
    job.keepAlive = false;
    job.start();
    try {
      const identity = await until("first start", BACKEND_MS, async () => {
        const value = await this.backend.identity();
        return {
          done: value?.backendKind === "rust",
          value,
          note: value ? `pid ${value.pid}` : "no answer",
        };
      });
      return `state created by backend ${identity.pid}`;
    } finally {
      await job.stop();
    }
  }

  async startJobs() {
    for (const name of ["supervisor", "recovery"]) {
      this.jobs[name] = new Job({
        name,
        argv: this.jobArguments[name],
        env: { ...this.env, XPC_SERVICE_NAME: `canary.${name}` },
        profile: this.profiles[name],
        log: path.join(this.root, "logs", `${name}.log`),
      });
      this.jobs[name].start();
      if (name === "supervisor") await sleep(2_000);
    }
    const identity = await this.waitBackend(this.builds.from, this.from);
    return `backend ${identity.pid}, ${identity.backendKind}, supervisor mode ${identity.supervisorMode}`;
  }

  async waitBackend(build, app, ms = BACKEND_MS) {
    const server = path.join(
      app,
      "Contents/Resources/workspace/bin/studio-runtime-server",
    );
    const hash = sha256File(server);
    const identity = await until(
      `backend ${build.commit.slice(0, 9)}`,
      ms,
      async () => {
        const value = await this.backend.identity();
        return {
          done:
            value?.backendKind === "rust" &&
            value.backendBuild === hash &&
            value.supervisorMode === true,
          value,
          note: value
            ? `pid ${value.pid} build ${String(value.backendBuild).slice(0, 8)}`
            : "no answer",
        };
      },
    );
    if (identity.stateDir !== fs.realpathSync(this.state))
      throw new CheckFailed(`the backend serves ${identity.stateDir}`);
    const session = await this.backend.get("/api/session");
    this.backend.token = session.token;
    return identity;
  }

  // --- scenario ----------------------------------------------------------------

  async models() {
    const rows = [];
    for (const account of this.accounts) {
      const listing = await this.backend.get(
        `/api/models?account_key=${encodeURIComponent(account.key)}`,
        120_000,
      );
      const models = listing?.data ?? [];
      if (models.length === 0)
        throw new CheckFailed(`/api/models lists nothing for ${account.key}`);
      rows.push(`${account.key}: ${models.length}`);
      account.models = models;
    }
    return rows.join(", ");
  }

  /** The backend's models.dev pricing artifact: the clone's, else the live state's (read only). */
  pricing() {
    for (const state of [this.state, this.liveState]) {
      const file = path.join(state, PRICING_ARTIFACT);
      if (fs.existsSync(file)) return readJSON(file);
    }
    return null;
  }

  chooseModel(account) {
    const pricing = this.pricing();
    // A model whose access programs lack "standard" needs Daybreak, which the canary never enables.
    const standard = account.models.filter((row) => {
      const programs = row?.availableAccessPrograms?.cyber;
      return !Array.isArray(programs) || programs.includes("standard");
    });
    const best = pricing && cheapestModel(standard, pricing, account.provider);
    if (best) return { model: best.model, effort: lowestEffort(best.efforts) };
    // Without the pricing artifact, the last visible model of the list (the catalog lists the largest first).
    const visible = standard.filter(
      (row) => !row.hidden && typeof row.model === "string",
    );
    if (visible.length === 0)
      throw new CheckFailed(`no visible model for ${account.key}`);
    const row = visible.at(-1);
    return {
      model: row.model,
      effort: lowestEffort(row.supportedReasoningEfforts ?? []),
    };
  }

  async chat(name, account, prompt) {
    const id = randomUUID();
    const { model, effort } = this.chooseModel(account);
    await this.backend.post("/api/agents", {
      id,
      name: `Canary ${this.runId} ${name}`,
      prompt,
      cwd: path.join(this.root, "work"),
      account_key: account.key,
      model,
      ...(effort ? { effort } : {}),
      role: "orchestrator",
      workspaceMode: "worktree",
    });
    this.chats[name] = { id, account: account.key, model, messages: [] };
    return this.chats[name];
  }

  async status(id) {
    const summary = await this.backend.get("/api/ui-summary");
    return (summary.chats ?? []).find((chat) => chat.id === id);
  }

  async transcript(id) {
    return (
      (
        await this.backend.get(
          `/api/transcript/page?id=${encodeURIComponent(id)}&limit=200`,
        )
      )?.items ?? []
    );
  }

  async answered(chat, token, ms = TURN_MS) {
    return until(`answer ${token}`, ms, async () => {
      const now = await this.status(chat.id);
      if (now && !now.inFlight && FAILED.has(now.status))
        throw new CheckFailed(`chat ${chat.id.slice(0, 8)} is ${now.status}`);
      const items = await this.transcript(chat.id);
      const answer = items.find(
        (item) => item.role === "assistant" && item.text?.includes(token),
      );
      return {
        done: Boolean(
          answer && now && !now.inFlight && TERMINAL.has(now.status),
        ),
        value: items,
        note: `status ${now?.status}`,
      };
    });
  }

  async running(chat, ms = TURN_MS) {
    return until(`chat ${chat.id.slice(0, 8)} running`, ms, async () => {
      const now = await this.status(chat.id);
      return {
        done: Boolean(now?.inFlight),
        value: now,
        note: `status ${now?.status}`,
      };
    });
  }

  async send(chat, text, delivery) {
    const message = { id: randomUUID(), room: chat.id, text, delivery };
    await this.backend.post("/api/messages", message);
    chat.messages.push(message.id);
    return message.id;
  }

  async startScenario() {
    const codex = this.accounts.find((account) => account.provider === "codex");
    const claude = this.accounts.find(
      (account) => account.provider === "claude",
    );
    // An idle chat: one finished turn before the install, one message after it.
    const idle = await this.chat(
      "idle",
      codex,
      `This is an automated Studio canary. Do not use tools. Reply with exactly ${this.token("IDLE")}.`,
    );
    await this.answered(idle, this.token("IDLE"));
    // A running Codex turn with a monitor that runs across the install, and a message queued behind it.
    const monitor = await this.chat(
      "monitor",
      codex,
      `This is an automated Studio canary. Use the orchestration_monitor tool once to run the shell command \`sleep ${MONITOR_SECONDS}; echo ${this.token("MON")}\`. Wait until Studio reports that the command exited, then reply with exactly ${this.token("MONITOR")}.`,
    );
    await this.running(monitor);
    this.queuedId = await this.send(
      monitor,
      `Reply with exactly ${this.token("QUEUED")}. Do not use tools.`,
      "next_turn",
    );
    // A running turn that gets a steer: the Claude CLI when the canary has a Claude login, else Codex.
    const steerAccount = claude ?? codex;
    const steered = await this.chat(
      "steer",
      steerAccount,
      // The Studio monitor tool needs no approval in either CLI; the turn waits for its exit.
      `This is an automated Studio canary. Use the orchestration_monitor tool once to run the shell command \`sleep ${STEER_SECONDS}\`. Wait until Studio reports that it exited, then reply with exactly ${this.token("STEER_BASE")}.`,
    );
    await this.running(steered);
    // Let the monitor start, so the steer lands in a running turn.
    await sleep(8_000);
    this.steerId = await this.send(
      steered,
      `Also add the word ${this.token("STEERED")} to your final reply.`,
      "current_turn",
    );
    return `idle, monitor (queued message), steer on ${steerAccount.key}`;
  }

  // --- install -----------------------------------------------------------------

  async install() {
    this.handlesBefore = openHandles(this.state);
    this.installStarted = Date.now();
    // The live install runs inside the recovery job; here the job is stopped first, so that its
    // update tick never runs build N's installer against /Applications.
    await this.jobs.recovery.stop();
    const build = this.builds.to;
    const staged = path.join(
      this.state,
      "updates",
      String(build.builtAt),
      APP_NAME,
    );
    fs.mkdirSync(path.dirname(staged), { recursive: true, mode: 0o700 });
    execFileSync("/usr/bin/ditto", [this.to, staged]);
    fs.writeFileSync(
      path.join(this.state, "pending-update.json"),
      JSON.stringify({
        id: randomUUID(),
        app: staged,
        build,
        from: `canary-${this.runId}`,
      }),
      { mode: 0o600 },
    );
    let output;
    try {
      output = execFileSync(
        "/usr/bin/sandbox-exec",
        [
          "-f",
          this.profiles.installer,
          this.python,
          "-B",
          path.join(HERE, "install.py"),
          "--resources",
          path.join(this.app, "Contents/Resources"),
          "--config",
          path.join(this.state, "background-recovery.json"),
          "--applications",
          path.join(this.root, "apps"),
        ],
        {
          cwd: "/",
          env: { ...this.env, PATH: LAUNCHD_PATH },
          encoding: "utf8",
          timeout: INSTALL_MS,
        },
      );
    } catch (error) {
      throw new CheckFailed(
        `the installer of build N failed: ${String(error.stdout || "")
          .trim()
          .split("\n")
          .at(-1)} ${String(error.stderr || "").slice(-400)}`,
      );
    }
    fs.writeFileSync(path.join(this.root, "logs", "installer.log"), output);
    const result = JSON.parse(output.trim().split("\n").at(-1));
    const receipt = readJSON(path.join(this.state, "update-receipt.json"));
    if (String(receipt.backend).startsWith("not restarted")) {
      const cause =
        output
          .split("\n")
          .find((line) => line.includes("canary-verify-failed")) ?? "";
      throw new CheckFailed(
        `installed, but the backend was ${receipt.backend} ${cause}`,
      );
    }
    // launchd starts the recovery job again from the swapped app; it starts the new server.
    this.jobs.recovery.keepAlive = true;
    this.jobs.recovery.start();
    const identity = await this.waitBackend(this.builds.to, this.to);
    return `installer ${result.detail}; backend ${identity.pid} on N+1 after ${((Date.now() - this.installStarted) / 1000).toFixed(0)} s`;
  }

  // --- checks ------------------------------------------------------------------

  async handles() {
    const after = openHandles(this.state);
    const changes = handleChanges(this.handlesBefore, after);
    if (changes.length)
      throw new CheckFailed(`provider handles changed: ${changes.join("; ")}`);
    if (after.length === 0)
      throw new CheckFailed("no provider handle was open");
    return `${after.length} handles kept pid and generation: ${after.join(", ")}`;
  }

  async deliveredOnce(chat, id, token) {
    const items = await this.answered(chat, token);
    const copies = messageItems(items, id);
    if (copies.length !== 1)
      throw new CheckFailed(
        `message ${id.slice(0, 8)} has ${copies.length} transcript copies`,
      );
    const receipts = await this.backend.get(
      `/api/messages/receipts?agent=${encodeURIComponent(chat.id)}&ids=${encodeURIComponent(JSON.stringify([id]))}`,
    );
    const receipt = (receipts?.items ?? []).find((item) => item.id === id);
    if (!receipt?.materialized)
      throw new CheckFailed(
        `message ${id.slice(0, 8)} receipt ${JSON.stringify(receipt)}`,
      );
    return `one copy, receipt ${receipt.status}`;
  }

  async monitorContinues() {
    await this.answered(this.chats.monitor, this.token("MONITOR"));
    const rows = query(
      path.join(this.state, "canvas.sqlite3"),
      `select json_extract(record,'$.status'), json_extract(record,'$.exitCode') from runtime_monitors where json_extract(record,'$.agent')='${this.chats.monitor.id}'`,
    );
    if (!rows.some(([status, code]) => status === "completed" && code === "0"))
      throw new CheckFailed(`monitor rows ${JSON.stringify(rows)}`);
    return "the monitor completed with exit 0 and the turn answered";
  }

  async steerOnce() {
    const chat = this.chats.steer;
    const detail = await this.deliveredOnce(
      chat,
      this.steerId,
      this.token("STEERED"),
    );
    return `steer ${detail}`;
  }

  /** The Claude rate-limit windows that the bridge recorded: five hours and a week mean a subscription. */
  claudeBuckets() {
    const [row] = query(
      path.join(this.state, "analytics.sqlite3"),
      "select record from analytics_limits where account='claude-canary' order by at desc limit 1",
    );
    if (!row)
      throw new CheckFailed(
        "the Claude account recorded no rate-limit windows",
      );
    const limits = JSON.parse(row[0])?.data?.rateLimits ?? {};
    const windows = [limits.primary, limits.secondary]
      .filter(Boolean)
      .map(
        (window) =>
          ({ 300: "five_hour", 10080: "weekly" })[window.windowDurationMins] ??
          `${window.windowDurationMins} min`,
      );
    if (!windows.includes("five_hour"))
      throw new CheckFailed(`Claude windows ${JSON.stringify(windows)}`);
    return `windows ${windows.join(", ")}`;
  }

  async idleContinues() {
    const chat = this.chats.idle;
    const id = await this.send(
      chat,
      `Reply with exactly ${this.token("AFTER")}. Do not use tools.`,
      "next_turn",
    );
    return this.deliveredOnce(chat, id, this.token("AFTER"));
  }

  logLines() {
    const since = this.installStarted;
    const files = [
      ...fs
        .readdirSync(path.join(this.root, "logs"))
        .map((name) => path.join(this.root, "logs", name)),
      ...fs
        .readdirSync(this.state)
        .filter((name) => name.endsWith(".log"))
        .map((name) => path.join(this.state, name)),
    ];
    const found = [];
    for (const file of files) {
      if (fs.statSync(file).mtimeMs < since) continue;
      for (const line of fs.readFileSync(file, "utf8").split("\n"))
        if (FAILURE_LINES.test(line))
          found.push(`${path.basename(file)}: ${line.slice(0, 200)}`);
    }
    if (found.length)
      throw new CheckFailed(
        `${found.length} failure lines, first: ${found[0]}`,
      );
    return `no failure line in ${files.length} logs`;
  }

  proxyReport() {
    const rows = [...this.proxy.seen]
      .sort()
      .map(([key, count]) => `${key} x${count}`);
    if (
      [...this.proxy.seen.keys()].some((key) =>
        key.startsWith("auth.openai.com"),
      )
    )
      throw new CheckFailed(
        `a provider asked for a token refresh (refused): ${rows.join(", ")}`,
      );
    return rows.join(", ") || "no request";
  }

  /** Tokens per chat from the analytics cost rows, priced by the models.dev artifact. */
  usage() {
    const pricing = this.pricing();
    const rows = [];
    let dollars = 0;
    let tokens = 0;
    for (const [name, chat] of Object.entries(this.chats)) {
      const provider = this.accounts.find(
        (account) => account.key === chat.account,
      )?.provider;
      const priced =
        pricing?.catalog?.providers?.[
          provider === "claude" ? "anthropic" : "openai"
        ]?.models ?? {};
      for (const [model, uncached, cached, written, output] of query(
        path.join(this.state, "analytics.sqlite3"),
        `select model, sum(uncached), sum(cached), sum(cache_write), sum(output) from analytics_cost_rows where agent='${chat.id}' group by model`,
      )) {
        const [u, c, w, o] = [uncached, cached, written, output].map(Number);
        tokens += u + c + w + o;
        const cost = priced[model]?.cost;
        const price = cost
          ? (u * cost.input +
              c * (cost.cache_read ?? cost.input) +
              w * (cost.cache_write ?? cost.input) +
              o * cost.output) /
            1e6
          : null;
        if (price !== null) dollars += price;
        rows.push(
          `${name} ${model}: ${u} in, ${c} cached, ${o} out${price === null ? "" : ` ($${price.toFixed(4)})`}`,
        );
      }
    }
    this.dollars = dollars;
    this.tokens = tokens;
    return `${rows.join("; ") || "no usage rows"}; ${tokens} tokens, about $${dollars.toFixed(4)} at API prices (the subscription logins are not billed per request)`;
  }

  liveUnchanged() {
    const now = liveFingerprint(this.liveState);
    const changed = Object.keys({ ...this.fingerprint, ...now }).filter(
      (file) => this.fingerprint[file] !== now[file],
    );
    if (changed.length)
      throw new CheckFailed(`the live side changed: ${changed.join(", ")}`);
    return "live recovery configuration, plists and app unchanged";
  }

  // --- cleanup -----------------------------------------------------------------

  async stopBackend() {
    const identity = this.backend && (await this.backend.identity());
    if (!identity?.pid || identity.stateDir !== fs.realpathSync(this.state))
      return;
    const command = execFileSync(
      "/bin/ps",
      ["-o", "command=", "-p", String(identity.pid)],
      {
        encoding: "utf8",
      },
    );
    if (
      !command.startsWith(fs.realpathSync(this.root)) &&
      !command.startsWith(this.root)
    )
      return;
    process.kill(identity.pid, "SIGTERM");
    await until("clone backend stop", 60_000, async () => {
      try {
        process.kill(identity.pid, 0);
        return { done: false, note: `pid ${identity.pid}` };
      } catch {
        return { done: true, value: true };
      }
    });
  }

  async cleanup() {
    for (const job of [this.jobs.recovery, this.jobs.supervisor])
      if (job) await job.stop().catch(() => {});
    // The recovery job leaves its server running, as launchd does: stop the clone server by its
    // verified identity (this state folder, an executable below the run folder).
    await this.stopBackend().catch(() => {});
    // Providers end when the supervisor closes their pipes; the canary signals none by PID.
    this.proxy?.server.close();
    if (this.keep || !this.root)
      return this.root ? `kept ${this.root}` : "nothing to remove";
    const marker = readJSON(path.join(this.root, RUN_MARKER));
    if (
      marker.runId !== this.runId ||
      path.dirname(fs.realpathSync(this.root)) !== this.temporaryRoot
    )
      throw new CheckFailed(`${this.root} is not this run's folder`);
    fs.rmSync(this.root, { recursive: true, force: false });
    return `removed ${this.root}`;
  }

  async main() {
    const report = this.report;
    const step = (name, check) => report.run(name, check);
    const started = Date.now();
    try {
      if (!(await step("prepare clone", () => this.prepare()))) return report;
      if (!(await step("accounts", () => this.accountsFile()))) return report;
      if (this.claudeProblem)
        await step("canary Claude login", () => {
          throw new CheckFailed(this.claudeProblem);
        });
      if (!(await step("sandbox and proxy", () => this.sandbox())))
        return report;
      if (
        !(await step("first start creates the state", () =>
          this.initializeState(),
        ))
      )
        return report;
      if (
        !(await step("configure build N (recovery helper)", () =>
          this.configure(),
        ))
      )
        return report;
      if (!(await step("start build N", () => this.startJobs()))) return report;
      if (!(await step("models on N", () => this.models()))) return report;
      if (!(await step("scenario on N", () => this.startScenario())))
        return report;
      if (!(await step("install N+1", () => this.install()))) return report;
      await step("provider handles kept", () => this.handles());
      await step("models on N+1", () => this.models());
      await step("monitor chat continues", () => this.monitorContinues());
      await step("queued message delivered once", () =>
        this.deliveredOnce(
          this.chats.monitor,
          this.queuedId,
          this.token("QUEUED"),
        ),
      );
      await step("steer delivered once", () => this.steerOnce());
      if (this.accounts.some((account) => account.provider === "claude"))
        await step("Claude counts against the subscription", () =>
          this.claudeBuckets(),
        );
      await step("idle chat continues", () => this.idleContinues());
      await step("no start or verify failure in logs", () => this.logLines());
    } finally {
      if (this.root) {
        if (this.proxy)
          await step("network through the proxy", () => this.proxyReport());
        await step("usage", () => this.usage());
        await step("live side unchanged", () => this.liveUnchanged());
        await step("cleanup", () => this.cleanup());
      }
      this.seconds = (Date.now() - started) / 1000;
    }
    return report;
  }
}

export function options(argv, environment = process.env) {
  if (
    environment.CANARY_CLAUDE_KEYCHAIN !== undefined &&
    !["0", "1"].includes(environment.CANARY_CLAUDE_KEYCHAIN)
  )
    throw new CheckFailed("Set CANARY_CLAUDE_KEYCHAIN to 1 or 0.");
  const { values } = parseArgs({
    args: argv,
    options: {
      from: { type: "string", default: DEFAULT_FROM },
      to: { type: "string" },
      "codex-account": { type: "string", default: "default" },
      "claude-config": {
        type: "string",
      },
      "claude-keychain": {
        type: "boolean",
        default: environment.CANARY_CLAUDE_KEYCHAIN === "1",
      },
      "claude-reservation": {
        type: "string",
        ...(environment.CANARY_CLAUDE_RESERVATION
          ? { default: environment.CANARY_CLAUDE_RESERVATION }
          : {}),
      },
      "no-claude": { type: "boolean", default: false },
      python: { type: "string", default: "/opt/homebrew/bin/python3" },
      node: { type: "string", default: process.execPath },
      "live-state": {
        type: "string",
        default: path.join(os.homedir(), ".local/state/codex-agents"),
      },
      keep: { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
  });
  values["claude-config"] ??=
    environment.CANARY_CLAUDE_CONFIG_DIR ||
    (values["claude-keychain"]
      ? undefined
      : path.join(os.homedir(), ".config/codex-studio/canary-claude"));
  return values;
}

const usage = `Usage: just upgrade-canary --to <N+1 app> [--from <N app>] [--codex-account key] [--claude-config dir] [--claude-keychain --claude-reservation record] [--keep]
Installs build N+1 over build N on an isolated clone with real processes; see docs/testing.md, "Upgrade canary".
`;

async function cli() {
  const parsed = options(process.argv.slice(2));
  if (parsed.help || !parsed.to) {
    process.stdout.write(usage);
    return parsed.help ? 0 : 2;
  }
  const canary = new Canary(parsed);
  process.stdout.write(
    `Studio upgrade canary ${canary.runId}: ${parsed.from} -> ${parsed.to}\n`,
  );
  const report = await canary.main();
  report.print();
  process.stdout.write(`time ${canary.seconds?.toFixed(0)} s\n`);
  return report.failed().length === 0 ? 0 : 1;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(fs.realpathSync(process.argv[1])).href
) {
  cli().then(
    (code) => process.exit(code),
    (error) => {
      process.stderr.write(
        `${error instanceof CheckFailed ? error.message : error.stack}\n`,
      );
      process.exit(2);
    },
  );
}
