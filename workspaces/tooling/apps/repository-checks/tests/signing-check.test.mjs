import test from "node:test";
import assert from "node:assert/strict";
import childProcess, { execFileSync } from "node:child_process";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { signingCheckReport } from "../../../../client/apps/desktop/signing-check.mjs";

const identity = "0123456789ABCDEF0123456789ABCDEF01234567";
const password = "fixture-password-that-must-never-appear";
const wrongPassphrase =
  "The user name or passphrase you entered is not correct.";
const desktop = fileURLToPath(
  new URL("../../../../client/apps/desktop/", import.meta.url),
);

// A temporary HOME holds signing.json, the password file and an empty keychain
// file. A fake security replaces the two security calls of signing.mjs.
function fixture(options = {}) {
  const root = mkdtempSync(path.join(os.tmpdir(), "signing-check-"));
  const config = path.join(root, ".config/codex-studio");
  mkdirSync(config, { recursive: true });
  const keychain = path.join(root, "Studio.keychain-db");
  const passwordFile = path.join(root, "password");
  writeFileSync(keychain, "", { mode: 0o600 });
  writeFileSync(passwordFile, `${password}\n`, { mode: 0o600 });
  writeFileSync(
    path.join(config, "signing.json"),
    options.json ?? JSON.stringify({ identity, keychain, passwordFile }),
    { mode: 0o600 },
  );
  return {
    root,
    keychain,
    passwordFile,
    configFile: path.join(config, "signing.json"),
  };
}

function withSecurity(
  { unlock = { status: 0, stderr: "" }, identities = identity } = {},
  callback,
) {
  const originals = {
    spawnSync: childProcess.spawnSync,
    execFileSync: childProcess.execFileSync,
  };
  const calls = { unlock: 0, find: 0, inputs: [], args: [] };
  childProcess.spawnSync = (command, args, options) => {
    calls.unlock++;
    calls.inputs.push(options.input);
    calls.args.push([command, ...args]);
    return unlock;
  };
  childProcess.execFileSync = (command, args) => {
    calls.find++;
    calls.args.push([command, ...args]);
    return identities;
  };
  syncBuiltinESMExports();
  try {
    return callback(calls);
  } finally {
    Object.assign(childProcess, originals);
    syncBuiltinESMExports();
  }
}

function inFixture(options, security, callback) {
  const files = fixture(options);
  const home = process.env.HOME;
  process.env.HOME = files.root;
  try {
    return withSecurity(security, (calls) => callback(files, calls));
  } finally {
    process.env.HOME = home;
    rmSync(files.root, { recursive: true, force: true });
  }
}

function assertNoSecrets(report) {
  assert.equal(report.line.includes(password), false);
}

test("passes with the keychain path and the identity fingerprint", () =>
  inFixture({}, {}, (files, calls) => {
    const report = signingCheckReport({});
    assert.deepEqual(report, {
      ok: true,
      line: `PASS: keychain ${files.keychain} identity ${identity}`,
    });
    assertNoSecrets(report, files);
    assert.equal(calls.unlock, 1);
    assert.equal(calls.find, 1);
    // The password reaches security only through stdin.
    assert.match(calls.inputs[0], /^unlock-keychain -p "fixture-password/);
    assert.equal(JSON.stringify(calls.args).includes(password), false);
  }));

test("passes with an identity from the environment and no keychain", () =>
  inFixture({}, {}, (files, calls) => {
    const report = signingCheckReport({
      CODEX_STUDIO_SIGNING_IDENTITY: identity,
    });
    assert.equal(
      report.line,
      `PASS: keychain (default search list) identity ${identity}`,
    );
    assert.equal(calls.unlock, 0);
  }));

test("fails when the configuration file is missing", () =>
  inFixture({}, {}, (files, calls) => {
    rmSync(files.configFile);
    const report = signingCheckReport({});
    assert.equal(report.ok, false);
    assert.match(
      report.line,
      new RegExp(
        `^FAIL: Signing configuration .*signing\\.json does not exist\\.`,
      ),
    );
    assert.match(report.line, /README\.md#signing-setup/);
    assert.equal(calls.unlock + calls.find, 0);
  }));

test("fails when the configuration is not valid JSON or has a bad identity", () => {
  inFixture({ json: "{" }, {}, () =>
    assert.match(signingCheckReport({}).line, /is not valid JSON/),
  );
  inFixture({ json: JSON.stringify({ identity: "abc" }) }, {}, () =>
    assert.match(
      signingCheckReport({}).line,
      /"identity" in .* must be a 40 character hexadecimal/,
    ),
  );
});

test("fails when the configuration file has a loose mode", () =>
  inFixture({}, {}, (files, calls) => {
    chmodSync(files.configFile, 0o644);
    const report = signingCheckReport({});
    assert.match(report.line, /has mode 0644\. Run: chmod 600 /);
    assert.equal(calls.unlock + calls.find, 0);
  }));

test("fails when the password file is missing or has a loose mode", () => {
  inFixture({}, {}, (files, calls) => {
    chmodSync(files.passwordFile, 0o640);
    const report = signingCheckReport({});
    assert.match(report.line, /Password file .* has mode 0640\./);
    assertNoSecrets(report, files);
    assert.equal(calls.unlock + calls.find, 0);
  });
  inFixture({}, {}, (files, calls) => {
    rmSync(files.passwordFile);
    assert.match(
      signingCheckReport({}).line,
      /Password file .*\/password does not exist\./,
    );
    assert.equal(calls.unlock, 0);
  });
});

test("fails when the keychain file is missing", () =>
  inFixture({}, {}, (files, calls) => {
    rmSync(files.keychain);
    assert.match(
      signingCheckReport({}).line,
      /Keychain file .*Studio\.keychain-db does not exist\./,
    );
    assert.equal(calls.unlock, 0);
  }));

test("fails with the security message when the unlock is refused", () => {
  const unlock = { status: 51, stderr: "" };
  return inFixture({}, { unlock }, (files, calls) => {
    unlock.stderr = `security: SecKeychainUnlock ${files.keychain}: ${wrongPassphrase}\nunlock-keychain: returned -25293\n`;
    const report = signingCheckReport({});
    assert.deepEqual(report, {
      ok: false,
      line: `FAIL: Could not unlock the dedicated Studio signing keychain ${files.keychain}: ${wrongPassphrase}\nSee workspaces/client/apps/native/README.md#signing-setup`,
    });
    assert.equal(calls.unlock, 1);
    assert.equal(calls.find, 0);
    assertNoSecrets(report, files);
  });
});

test("fails when the identity is not in the keychain", () =>
  inFixture(
    {},
    { identities: "     0 valid identities found\n" },
    (files, calls) => {
      const report = signingCheckReport({});
      assert.equal(report.ok, false);
      assert.match(
        report.line,
        new RegExp(`certificate ${identity} is not in keychain `),
      );
      assert.equal(calls.unlock, 1);
      assertNoSecrets(report, files);
    },
  ));

test("the unlock error names the security cause and hides the password", () => {
  const output = execFileSync(
    process.execPath,
    [path.join(desktop, "signing-test.mjs"), "--unlock-contract"],
    { encoding: "utf8" },
  );
  assert.match(output, /^PASS: /);
});
