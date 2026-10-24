import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { test } from "node:test";
import { Canary } from "../canary.mjs";
import {
  reserveStudioProfile,
  bindStudioProfile,
  studioProfile,
} from "../studio-claude-profile.mjs";

const login = "19c1c14e-1919-4c14-9191-191919191919";
const account = "claude-profile-0123456789abcdef0123";
function fixture(t) {
  const root = fs.realpathSync(
    fs.mkdtempSync(path.join(os.tmpdir(), "canary-studio-profile-")),
  );
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const live = path.join(root, "live");
  const storage = path.join(root, "ownership");
  for (const folder of [
    live,
    storage,
    path.join(live, "accounts"),
    path.join(live, "claude-logins"),
  ])
    fs.mkdirSync(folder, { mode: 0o700 });
  const registry = path.join(live, "accounts/registry.json");
  fs.writeFileSync(registry, JSON.stringify({ accounts: {} }), { mode: 0o600 });
  const sql = (name, text) =>
    execFileSync("/usr/bin/sqlite3", [path.join(live, name), text]);
  sql(
    "canvas.sqlite3",
    "CREATE TABLE runtime_agents(record TEXT,account_key TEXT GENERATED ALWAYS AS (json_extract(record,'$.accountKey')) VIRTUAL); CREATE INDEX runtime_agent_account ON runtime_agents(account_key); CREATE TABLE runtime_execution_runs(account TEXT); CREATE TABLE runtime_account_transfers(record TEXT);",
  );
  sql(
    "analytics.sqlite3",
    "CREATE TABLE analytics_agents(record TEXT); CREATE TABLE analytics_usage(record TEXT);",
  );
  sql(
    "supervisor.sqlite3",
    "CREATE TABLE handles(id TEXT PRIMARY KEY,closed_at REAL);",
  );
  const record = path.join(storage, "reservation.json");
  const binary = path.join(root, "native-claude");
  fs.writeFileSync(binary, "native fixture", { mode: 0o700 });
  const config = path.join(live, "claude-logins", `config-${login}`);
  const options = { storage };
  const reserve = () => reserveStudioProfile(record, live, options);
  const added = () => {
    fs.mkdirSync(config, { mode: 0o700 });
    fs.writeFileSync(
      registry,
      JSON.stringify({
        accounts: {
          ...JSON.parse(fs.readFileSync(registry, "utf8")).accounts,
          [account]: {
            id: account,
            provider: "claude",
            home: config,
            email: "canary@example.test",
            accountId: "claude:canary@example.test",
            _credentialIdentity: "claude:canary@example.test",
            status: "ready",
            deleted: true,
            claudeOptions: { configDir: config, binaryPath: binary },
          },
        },
      }),
      { mode: 0o600 },
    );
    fs.writeFileSync(
      path.join(live, "claude-logins", login + ".json"),
      JSON.stringify({
        receipt: {
          requestId: login,
          accountKey: account,
          status: "ready",
          email: "canary@example.test",
          chatsRefreshed: true,
        },
        metadata: {
          flow: "add",
          configDir: config,
          binaryPath: binary,
          cleanupDone: true,
        },
      }),
      { mode: 0o600 },
    );
  };
  return {
    root,
    live,
    record,
    binary,
    config,
    options,
    reserve,
    added,
    sql,
    registry,
    bind: () => bindStudioProfile(record, login, options),
    read: () => studioProfile(record, live, options),
  };
}

test("Studio canary admission binds only a fresh excluded Add profile and preserves its config identity", (t) => {
  const f = fixture(t);
  f.reserve();
  f.added();
  // Neither reserve nor bind needs a native credential or account file.
  f.bind();
  const result = f.read();
  assert.equal(result.config, f.config);
  assert.equal(result.binary, f.binary);
  assert.equal(result.account.id, account);
  assert.equal(fs.statSync(f.record).mode & 0o777, 0o600);
  f.bind();
  assert.equal(f.read().config, f.config);
});
test("reservation retry preserves the original baseline and binding cannot select an existing profile", (t) => {
  const f = fixture(t);
  f.added();
  f.reserve();
  const before = fs.readFileSync(f.record, "utf8");
  f.reserve();
  assert.equal(fs.readFileSync(f.record, "utf8"), before);
  assert.throws(f.bind);
});
test("ready live profiles cannot be bound or re-enabled after binding", (t) => {
  const f = fixture(t);
  f.reserve();
  f.added();
  f.bind();
  const registry = JSON.parse(fs.readFileSync(f.registry));
  registry.accounts[account].deleted = false;
  registry.accounts[account].disconnected = true;
  fs.writeFileSync(f.registry, JSON.stringify(registry));
  assert.throws(f.read);
});
for (const [name, sql] of [
  [
    "live chat",
    `INSERT INTO runtime_agents VALUES('{"accountKey":"${account}"}');`,
  ],
  ["execution", `INSERT INTO runtime_execution_runs VALUES('${account}');`],
  [
    "transfer",
    `INSERT INTO runtime_account_transfers VALUES('{"targetAccountKey":"${account}"}');`,
  ],
  [
    "analytics chat",
    `INSERT INTO analytics_agents VALUES('{"accountKey":"${account}"}');`,
  ],
  [
    "usage",
    `INSERT INTO analytics_usage VALUES('{"accountKey":"${account}"}');`,
  ],
  ["active provider", `INSERT INTO handles VALUES('account:${account}',NULL);`],
])
  test(`a fresh profile with a ${name} footprint is refused`, (t) => {
    const f = fixture(t);
    f.reserve();
    f.added();
    if (!name.endsWith("provider"))
      f.sql(
        "supervisor.sqlite3",
        `INSERT INTO handles VALUES('account:${account}',1);`,
      );
    f.sql(
      name === "live chat" || name === "execution" || name === "transfer"
        ? "canvas.sqlite3"
        : name.endsWith("provider")
          ? "supervisor.sqlite3"
          : "analytics.sqlite3",
      sql,
    );
    assert.throws(f.bind);
  });
test("a fresh excluded profile may retain a closed automatic metadata SDK handle", (t) => {
  const f = fixture(t);
  f.reserve();
  f.added();
  f.sql(
    "supervisor.sqlite3",
    `INSERT INTO handles VALUES('account:${account}',1);`,
  );
  assert.doesNotThrow(f.bind);
  assert.equal(f.read().config, f.config);
});
test("the real canary registry and native environment retain the bound Studio identity", (t) => {
  const f = fixture(t),
    codex = path.join(f.root, "codex");
  fs.mkdirSync(codex);
  fs.writeFileSync(path.join(codex, "auth.json"), "fixture only");
  fs.writeFileSync(
    f.registry,
    JSON.stringify({
      accounts: { default: { provider: "codex", home: codex } },
    }),
  );
  f.reserve();
  f.added();
  f.bind();
  const run = path.join(f.root, "run");
  fs.mkdirSync(path.join(run, "homes"), { recursive: true });
  fs.mkdirSync(path.join(run, "state"));
  const canary = new Canary({
    from: f.root,
    to: f.root,
    "codex-account": "default",
    "claude-keychain": true,
    "claude-reservation": f.record,
    "live-state": f.live,
  });
  canary.root = run;
  canary.claudeOwnershipStorage = f.options.storage;
  canary.accountsFile();
  const own = JSON.parse(
    fs.readFileSync(path.join(run, "state/accounts/registry.json")),
  ).accounts["claude-canary"];
  assert.equal(own.home, f.config);
  assert.deepEqual(own.claudeOptions, {
    configDir: f.config,
    binaryPath: f.binary,
  });
  assert.equal(own._credentialIdentity, "claude:canary@example.test");
  assert.equal(
    canary.accounts.find((row) => row.key === "claude-canary").accountId,
    own.accountId,
  );
  assert.deepEqual(canary.claudeEnvironment(), {
    CLAUDE_CONFIG_DIR: f.config,
    CLAUDE_SECURESTORAGE_CONFIG_DIR: f.config,
  });
});
test("a scan plan or excessive new metadata window cannot become admission evidence", (t) => {
  const scanned = fixture(t);
  scanned.reserve();
  scanned.added();
  scanned.sql("canvas.sqlite3", "DROP INDEX runtime_agent_account;");
  assert.throws(scanned.bind);
  const grown = fixture(t);
  grown.reserve();
  grown.added();
  grown.sql(
    "canvas.sqlite3",
    "INSERT INTO runtime_execution_runs(rowid,account) VALUES(100001,'another-account');",
  );
  assert.throws(grown.bind);
});
test("missing evidence, another native binary, and a copied config identity are refused", (t) => {
  const f = fixture(t);
  f.reserve();
  f.added();
  f.bind();
  fs.appendFileSync(f.binary, "changed");
  assert.throws(f.read);
  const fresh = fixture(t);
  fresh.reserve();
  fresh.added();
  fresh.bind();
  fs.renameSync(fresh.config, fresh.config + "-moved");
  fs.mkdirSync(fresh.config, { mode: 0o700 });
  assert.throws(fresh.read);
  const missing = fixture(t);
  missing.reserve();
  missing.added();
  fs.unlinkSync(path.join(missing.live, "canvas.sqlite3"));
  assert.throws(missing.bind);
});
