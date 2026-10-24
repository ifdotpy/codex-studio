import assert from "node:assert/strict";
import { test } from "node:test";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { execFileSync } from "node:child_process";
import { EventEmitter } from "node:events";
import {
  keychainConfig,
  prepareKeychainConfig,
  nativeLoginSpec,
  fileConfig,
  claudeAccount,
} from "../claude-auth.mjs";

import {
  FAILURE_LINES,
  handleChanges,
  profiles,
  proxyDecision,
  Canary,
  options,
} from "../canary.mjs";
import { loginProfile, runNativeLogin } from "../claude-login.mjs";

function authFixture(t) {
  const root = fs.realpathSync(
    fs.mkdtempSync(path.join(os.tmpdir(), "canary-auth-test-")),
  );
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  fs.mkdirSync(home, { mode: 0o700 });
  const config = path.join(
    home,
    ".config/codex-studio/canary-claude-keychain/native",
  );
  return { root, home, config };
}

test("Keychain profiles require a private dedicated marked path without aliases", (t) => {
  const { home, config } = authFixture(t);
  assert.throws(() => keychainConfig(config, { home }));
  assert.throws(() =>
    prepareKeychainConfig(path.join(home, ".claude"), { home }),
  );
  prepareKeychainConfig(config, { home });
  assert.equal(keychainConfig(config, { home }), config);
  fs.chmodSync(config, 0o755);
  assert.throws(() => keychainConfig(config, { home }));
  fs.chmodSync(config, 0o700);
  const alias = config + "-alias";
  fs.symlinkSync(config, alias);
  assert.throws(() => keychainConfig(alias, { home }));
  assert.throws(() => prepareKeychainConfig(alias, { home }));
  fs.writeFileSync(
    path.join(config, "canary-auth.json"),
    JSON.stringify({ version: 1, config: alias, uid: process.getuid() }),
  );
  assert.throws(() => keychainConfig(config, { home }));
});

test("a missing file login remains a missing prerequisite without enabling Keychain", (t) => {
  const { home } = authFixture(t);
  const config = path.join(home, ".config/codex-studio/canary-claude");
  assert.equal(fileConfig(config, { home }), config);
  assert.throws(() => fileConfig(path.join(home, ".claude"), { home }));
});

test("malformed private account metadata never exposes its contents in an error", (t) => {
  const { home, config } = authFixture(t);
  prepareKeychainConfig(config, { home });
  fs.writeFileSync(path.join(config, ".claude.json"), "SENSITIVE_CLAUDE_VALUE");
  assert.throws(
    () => claudeAccount(config),
    (error) => !error.message.includes("SENSITIVE"),
  );
});

test("Keychain access requires an explicit flag or exact environment opt-in", () => {
  assert.equal(options([], {})["claude-keychain"], false);
  assert.equal(
    options([], { CANARY_CLAUDE_KEYCHAIN: "1" })["claude-keychain"],
    true,
  );
  assert.equal(
    options([], { CANARY_CLAUDE_KEYCHAIN: "0" })["claude-keychain"],
    false,
  );
  assert.equal(options(["--claude-keychain"], {})["claude-keychain"], true);
  assert.throws(() => options([], { CANARY_CLAUDE_KEYCHAIN: "true" }));
  assert.match(
    options([], { CANARY_CLAUDE_KEYCHAIN: "1" })["claude-config"],
    /canary-claude-keychain\/native$/,
  );
});

test("disabled Claude and a missing file login never select the live discovery profile", (t) => {
  const { root, home } = authFixture(t);
  const config = path.join(home, ".config/codex-studio/canary-claude");
  const base = {
    from: root,
    to: root,
    "claude-config": config,
    "live-state": path.join(root, "live"),
  };
  const missing = new Canary(base);
  missing.root = root;
  assert.deepEqual(missing.claudeEnvironment(), {
    CLAUDE_CONFIG_DIR: config,
    CLAUDE_SECURESTORAGE_CONFIG_DIR: config,
  });
  const disabled = new Canary({ ...base, "no-claude": true });
  disabled.root = root;
  assert.deepEqual(disabled.claudeEnvironment(), {
    CLAUDE_CONFIG_DIR: path.join(root, "homes/claude-disabled"),
    CLAUDE_SECURESTORAGE_CONFIG_DIR: path.join(root, "homes/claude-disabled"),
  });
});

test("native login adds only an inbound local OAuth callback to the provider sandbox", () => {
  const args = {
    root: path.join(os.tmpdir(), "canary"),
    tmp: os.tmpdir(),
    home: "/Users/owner",
    liveState: "/Users/owner/state",
    app: path.join(os.tmpdir(), "canary/app"),
    port: null,
    proxyPort: 4900,
    readable: [],
  };
  const text = loginProfile(args);
  assert.match(text, /\(allow network-bind \(local tcp "localhost:\*"\)\)/);
  assert.match(text, /\(allow network-inbound \(local tcp "localhost:\*"\)\)/);
  assert.doesNotMatch(text, /remote tcp "localhost:\*"/);
  assert.doesNotMatch(text, /remote tcp "localhost:null"/);
  assert.equal(
    text.split('\n(allow network-bind (local tcp "localhost:*"))')[0],
    profiles({ ...args, claudeKeychain: true }).supervisor,
  );
});

test(
  "the native login sandbox passes exact identity to a fake full-auth CLI",
  { skip: process.platform !== "darwin" },
  (t) => {
    const { home, config, root } = authFixture(t);
    prepareKeychainConfig(config, { home });
    const executable = path.join(root, "fake-claude");
    fs.writeFileSync(
      executable,
      `#!${process.execPath}\nprocess.stdout.write(JSON.stringify({args:process.argv.slice(2),config:process.env.CLAUDE_CONFIG_DIR,storage:process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR,token:process.env.CLAUDE_CODE_OAUTH_TOKEN??null}));\n`,
      { mode: 0o700 },
    );
    const profile = path.join(root, "login.sb");
    fs.writeFileSync(
      profile,
      loginProfile({
        root,
        tmp: root,
        home,
        liveState: path.join(root, "live"),
        app: "/Applications/Codex Studio.app",
        port: null,
        proxyPort: 4900,
        readable: [],
        writable: [config],
      }),
    );
    const spec = nativeLoginSpec({
      config,
      home,
      tmp: root,
      profile,
      executable,
      proxyURL: "http://127.0.0.1:4900",
    });
    const actual = JSON.parse(
      execFileSync(spec.file, spec.args, {
        cwd: spec.cwd,
        env: spec.env,
        encoding: "utf8",
        timeout: 10_000,
      }),
    );
    assert.deepEqual(actual, {
      args: ["auth", "login", "--claudeai"],
      config,
      storage: config,
      token: null,
    });
  },
);

test("native login cancellation stops its owned child without a second login", async () => {
  const signals = new EventEmitter();
  const child = new EventEmitter();
  const killed = [];
  let starts = 0;
  child.kill = (signal) => {
    killed.push(signal);
    queueMicrotask(() => child.emit("exit", 0));
  };
  const pending = runNativeLogin(
    {
      file: "native",
      args: ["auth", "login"],
      env: {},
      cwd: "/private/profile",
    },
    (_file, _args, options) => {
      starts++;
      assert.equal(options.cwd, "/private/profile");
      return child;
    },
    signals,
  );
  signals.emit("SIGTERM");
  assert.equal(await pending, 1);
  assert.equal(starts, 1);
  assert.deepEqual(killed, ["SIGTERM"]);
  assert.equal(signals.listenerCount("SIGTERM"), 0);
});

test("native login uses full auth and exact profile identity without inherited credentials", (t) => {
  const { root, home, config } = authFixture(t);
  prepareKeychainConfig(config, { home });
  const spec = nativeLoginSpec({
    config,
    home,
    tmp: root,
    profile: path.join(root, "login.sb"),
    executable: "/native/claude",
    proxyURL: "http://127.0.0.1:4900",
  });
  assert.equal(spec.file, "/usr/bin/sandbox-exec");
  assert.deepEqual(spec.args.slice(-4), [
    "/native/claude",
    "auth",
    "login",
    "--claudeai",
  ]);
  for (const name of ["CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR"])
    assert.equal(spec.env[name], config);
  for (const name of [
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "NODE_OPTIONS",
  ])
    assert.equal(spec.env[name], undefined);
  const observed = JSON.parse(
    execFileSync(
      process.execPath,
      [
        "-e",
        "process.stdout.write(JSON.stringify({config:process.env.CLAUDE_CONFIG_DIR,storage:process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR,proxy:process.env.HTTPS_PROXY}))",
      ],
      { env: spec.env, encoding: "utf8" },
    ),
  );
  assert.deepEqual(observed, {
    config,
    storage: config,
    proxy: "http://127.0.0.1:4900",
  });
});

test("Keychain mode grants securityd only to provider and recovery profiles", () => {
  const args = {
    root: path.join(os.tmpdir(), "canary"),
    tmp: os.tmpdir(),
    home: "/Users/owner",
    liveState: "/Users/owner/state",
    app: path.join(os.tmpdir(), "canary/app"),
    port: 4800,
    proxyPort: 4900,
    readable: [],
  };
  const locked = profiles(args);
  const allowed = profiles({ ...args, claudeKeychain: true });
  for (const name of ["supervisor", "recovery"]) {
    assert.match(locked[name], /\(deny mach-lookup .*com.apple.securityd/);
    assert.doesNotMatch(
      allowed[name],
      /\(deny mach-lookup .*com.apple.securityd/,
    );
    assert.equal(
      allowed[name],
      locked[name].replace(/\n\(deny mach-lookup[^\n]+/, ""),
    );
  }
  assert.equal(allowed.installer, locked.installer);
});

test("a Keychain Claude registry uses the exact config identity without a credential file", (t) => {
  const { root, home, config } = authFixture(t);
  prepareKeychainConfig(config, { home });
  fs.writeFileSync(
    path.join(config, ".claude.json"),
    JSON.stringify({ oauthAccount: { emailAddress: "canary@example.test" } }),
  );
  const liveState = path.join(root, "live");
  const codex = path.join(root, "codex");
  fs.mkdirSync(path.join(liveState, "accounts"), { recursive: true });
  fs.mkdirSync(codex);
  fs.writeFileSync(path.join(codex, "auth.json"), "fixture");
  fs.writeFileSync(
    path.join(liveState, "accounts/registry.json"),
    JSON.stringify({
      accounts: { default: { provider: "codex", home: codex } },
    }),
  );
  const run = path.join(root, "run");
  fs.mkdirSync(path.join(run, "homes"), { recursive: true });
  fs.mkdirSync(path.join(run, "state"));
  const canary = new Canary({
    from: root,
    to: root,
    "codex-account": "default",
    "claude-config": config,
    "claude-keychain": true,
    "live-state": liveState,
  });
  canary.root = run;
  // The production constructor uses the real owner's private namespace.
  // This fixture has a separate owner home and never reads an actual login.
  canary.claudeProfileHome = home;
  canary.accountsFile();
  assert.equal(canary.claudeProblem, undefined);
  const account = JSON.parse(
    fs.readFileSync(path.join(run, "state/accounts/registry.json")),
  ).accounts["claude-canary"];
  assert.equal(account.home, config);
  assert.equal(account.claudeOptions.configDir, config);
  assert.equal(account.email, "canary@example.test");
  assert.deepEqual(
    canary.accounts.map((row) => row.provider),
    ["codex", "claude"],
  );
});

test("the proxy refuses the Codex token endpoint and every unknown host", () => {
  assert.equal(proxyDecision("auth.openai.com"), "refused");
  assert.equal(proxyDecision("AUTH.OPENAI.COM."), "refused");
  assert.equal(proxyDecision("chatgpt.com"), "allowed");
  assert.equal(proxyDecision("api.openai.com"), "allowed");
  assert.equal(proxyDecision("api.anthropic.com"), "allowed");
  assert.equal(proxyDecision("example.com"), "refused");
  assert.equal(proxyDecision("chatgpt.com.example.com"), "refused");
});

test("a handle with another pid or generation is a change", () => {
  const before = ["account:a|10|1", "account:b|20|4"];
  assert.deepEqual(handleChanges(before, [...before]), []);
  assert.deepEqual(
    handleChanges(before, ["account:a|10|1", "account:b|21|5"]),
    ["- account:b|20|4", "+ account:b|21|5"],
  );
  assert.deepEqual(handleChanges(before, ["account:a|10|1"]), [
    "- account:b|20|4",
  ]);
});

test("the failure lines match both outage messages", () => {
  assert.match("Provider did not start: refused", FAILURE_LINES);
  assert.match("The retained handle could not be verified", FAILURE_LINES);
  assert.doesNotMatch("turn started", FAILURE_LINES);
});

test("every canary sandbox keeps the live side, the Keychain and the network out", () => {
  const text = profiles({
    root: path.join(os.tmpdir(), "studio-canary-x"),
    tmp: os.tmpdir(),
    home: "/Users/owner",
    liveState: "/Users/owner/.local/state/codex-agents",
    app: path.join(os.tmpdir(), "studio-canary-x/apps/Codex Studio.app"),
    port: 4800,
    proxyPort: 4900,
    readable: ["/Users/owner/.codex/auth.json"],
  });
  for (const [name, profile] of Object.entries(text)) {
    assert.match(profile, /\(deny default\)/, name);
    assert.match(profile, /global-name "com.apple.SecurityServer"/, name);
    assert.match(
      profile,
      /\(deny file-write\* \(subpath "\/Users\/owner\/.local\/state\/codex-agents"\) \(subpath "\/Applications"\)/,
      name,
    );
    assert.match(
      profile,
      /\(deny file-write\* \(literal "\/Users\/owner\/.codex\/auth.json"\)\)/,
      name,
    );
    assert.match(
      profile,
      /\(deny process-exec \(subpath "\/Applications\/Codex Studio.app"\)/,
      name,
    );
    assert.match(profile, /\(remote tcp "localhost:4900"\)/, name);
    assert.doesNotMatch(profile, /\(allow network-outbound\)/, name);
  }
  assert.match(
    text.installer,
    /\(deny process-exec \(literal "\/usr\/bin\/osascript"\) \(literal "\/usr\/bin\/open"\)\)/,
  );
});
