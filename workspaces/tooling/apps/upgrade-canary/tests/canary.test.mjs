import assert from "node:assert/strict";
import "./studio-profile.test.mjs";
import { test } from "node:test";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { execFileSync } from "node:child_process";
import {
  fileConfig,
  claudeAccount,
  claudeConfigEnvironment,
} from "../claude-auth.mjs";
import { cli as admissionCli } from "../claude-login.mjs";
import {
  FAILURE_LINES,
  handleChanges,
  profiles,
  proxyDecision,
  Canary,
  options,
} from "../canary.mjs";

function temporary(t) {
  const root = fs.realpathSync(
    fs.mkdtempSync(path.join(os.tmpdir(), "canary-profile-boundary-")),
  );
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return root;
}
test("the admission helper cannot start native authentication", () => {
  assert.throws(() => admissionCli(["--login", "--claude", "/native/claude"]));
  assert.throws(() => admissionCli(["--prepare"]));
});
test("Keychain access requires the explicit mode and a bound Studio reservation", () => {
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
  const parsed = options([], {
    CANARY_CLAUDE_KEYCHAIN: "1",
    CANARY_CLAUDE_RESERVATION: "/private/ownership.json",
  });
  assert.equal(parsed["claude-reservation"], "/private/ownership.json");
  assert.equal(parsed["claude-config"], undefined);
  assert.throws(
    () =>
      new Canary({
        from: os.tmpdir(),
        to: os.tmpdir(),
        "live-state": path.join(os.tmpdir(), "state"),
        "claude-keychain": true,
      }),
  );
});
test("all canary modes pin native discovery to the selected or empty config", (t) => {
  const root = temporary(t),
    config = path.join(root, "private-config");
  const base = {
    from: root,
    to: root,
    "claude-config": config,
    "live-state": path.join(root, "live"),
  };
  const missing = new Canary(base);
  missing.root = root;
  assert.deepEqual(
    missing.claudeEnvironment(),
    claudeConfigEnvironment(config),
  );
  const disabled = new Canary({ ...base, "no-claude": true });
  disabled.root = root;
  assert.deepEqual(
    disabled.claudeEnvironment(),
    claudeConfigEnvironment(path.join(root, "homes/claude-disabled")),
  );
  assert.deepEqual(Object.keys(missing.claudeEnvironment()).sort(), [
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
  ]);
});
test("file mode keeps its missing prerequisite and refuses production or aliased paths", (t) => {
  const home = temporary(t),
    config = path.join(home, ".config/codex-studio/canary-claude");
  assert.equal(fileConfig(config, { home }), config);
  assert.throws(() => fileConfig(path.join(home, ".claude"), { home }));
  fs.mkdirSync(config, { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(config, ".claude.json"), "SENSITIVE_CLAUDE_VALUE");
  assert.throws(
    () => claudeAccount(config),
    (e) => !e.message.includes("SENSITIVE"),
  );
  fs.symlinkSync(
    "/missing-native-credentials",
    path.join(config, ".credentials.json"),
  );
  assert.throws(() => fileConfig(config, { home }));
});
test("Keychain mode changes only service access and the exact dedicated write rule", () => {
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
  const locked = profiles(args),
    config = path.join(args.liveState, "claude-logins/config-fixture");
  const allowed = profiles({
    ...args,
    claudeKeychain: true,
    claudeProfile: config,
  });
  for (const name of ["supervisor", "recovery"]) {
    assert.equal(
      allowed[name],
      locked[name].replace(/\n\(deny mach-lookup[^\n]+/, "") +
        `\n(allow file-write* (subpath "${config}"))`,
    );
  }
  assert.equal(allowed.installer, locked.installer);
});
test(
  "the actual sandbox permits the dedicated config but denies sibling and symlink writes",
  { skip: process.platform !== "darwin" },
  (t) => {
    const root = temporary(t),
      live = path.join(root, "live"),
      config = path.join(live, "claude-logins/config-fixture"),
      run = path.join(root, "run");
    fs.mkdirSync(config, { recursive: true, mode: 0o700 });
    fs.mkdirSync(run);
    const good = path.join(config, "fixture"),
      sibling = path.join(live, "never-touch");
    const escape = path.join(config, "escape");
    fs.symlinkSync(sibling, escape);
    const text = profiles({
      root: run,
      tmp: root,
      home: root,
      liveState: live,
      app: path.join(run, "app"),
      port: 4800,
      proxyPort: 4900,
      readable: [],
      claudeKeychain: true,
      claudeProfile: config,
    }).supervisor;
    execFileSync(
      "/usr/bin/sandbox-exec",
      [
        "-p",
        text,
        "/bin/sh",
        "-c",
        'printf fixture > "$1" && ! (printf forbidden > "$2") && ! (printf forbidden > "$3")',
        "fixture",
        good,
        sibling,
        escape,
      ],
      { encoding: "utf8", timeout: 10_000, stdio: ["ignore", "pipe", "pipe"] },
    );
    assert.equal(fs.readFileSync(good, "utf8"), "fixture");
    assert.equal(fs.existsSync(sibling), false);
  },
);
test("the proxy refuses the Codex token endpoint and every unknown host", () => {
  assert.equal(proxyDecision("auth.openai.com"), "refused");
  assert.equal(proxyDecision("AUTH.OPENAI.COM."), "refused");
  for (const host of ["chatgpt.com", "api.openai.com", "api.anthropic.com"])
    assert.equal(proxyDecision(host), "allowed");
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
test("default canary profiles keep live files, Keychain and direct network access denied", () => {
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
