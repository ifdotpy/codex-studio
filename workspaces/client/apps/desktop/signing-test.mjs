// Real macOS signatures and Python imports, with an isolated probe bundle.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import childProcess from "node:child_process";
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import {
  mkdtempSync,
  mkdirSync,
  copyFileSync,
  chmodSync,
  writeFileSync,
  readdirSync,
  readFileSync,
  statSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { signApplication, signingIdentity } from "./signing.mjs";

function privateUnlockContract() {
  const originals = {
    spawnSync: childProcess.spawnSync,
    execFileSync: childProcess.execFileSync,
    readFileSync: fs.readFileSync,
  };
  const identity = "A".repeat(40);
  const password = `synthetic "value"\\with ' space;$()`;
  let secret = password;
  let status = 0;
  let calls = 0;
  const keychain = `/fixture/Studio "safe"\\name.keychain-db`;
  fs.readFileSync = (filename, options) =>
    filename === "/fixture/password"
      ? secret
      : String(filename).endsWith("codex-studio/signing.json")
        ? JSON.stringify({
            identity,
            keychain,
            passwordFile: "/fixture/password",
          })
        : originals.readFileSync(filename, options);
  childProcess.spawnSync = (command, args, options) => {
    calls++;
    assert.equal(command, "/usr/bin/security");
    assert.deepEqual(args, ["-i"]);
    assert.equal(options.stdio, "pipe");
    assert.equal(options.timeout, 10_000);
    assert.equal(
      options.input,
      String.raw`unlock-keychain -p "synthetic \"value\"\\with ' space;$()" "/fixture/Studio \"safe\"\\name.keychain-db"` +
        "\n",
    );
    assert.equal(JSON.stringify(args).includes(secret), false);
    return { status, stdout: secret, stderr: secret };
  };
  childProcess.execFileSync = (command, args) => {
    assert.equal(command, "security");
    assert.deepEqual(args, ["find-identity", "-p", "codesigning", keychain]);
    return identity;
  };
  syncBuiltinESMExports();
  try {
    assert.equal(signingIdentity({}), identity);
    status = 1;
    assert.throws(
      () => signingIdentity({}),
      (error) =>
        error.message ===
          "Could not unlock the dedicated Studio signing keychain." &&
        !String(error.stack).includes(password),
    );
    const previousCalls = calls;
    secret = "synthetic\nfind-generic-password";
    assert.throws(
      () => signingIdentity({}),
      /Invalid dedicated Studio signing input/,
    );
    secret = "x".repeat(4096);
    assert.throws(
      () => signingIdentity({}),
      /Invalid dedicated Studio signing input/,
    );
    assert.equal(calls, previousCalls);
  } finally {
    Object.assign(childProcess, {
      spawnSync: originals.spawnSync,
      execFileSync: originals.execFileSync,
    });
    fs.readFileSync = originals.readFileSync;
    syncBuiltinESMExports();
  }
}
privateUnlockContract();
if (process.argv.includes("--unlock-contract")) {
  console.log("PASS: private stdin unlock, literal arguments and safe failure");
} else {
  const identity = signingIdentity();
  const root = mkdtempSync(path.join(tmpdir(), "studio-signature-"));
  const app = path.join(root, "Probe.app");
  const scripts = path.join(app, "Contents/Resources/workspace/scripts");
  const repositoryScripts = fileURLToPath(
    new URL("../scripts", import.meta.url),
  );
  const lease = path.join(scripts, ".studio-update.lock");
  try {
    mkdirSync(path.join(app, "Contents/MacOS"), { recursive: true });
    mkdirSync(scripts, { recursive: true });
    copyFileSync("/usr/bin/true", path.join(app, "Contents/MacOS/Probe"));
    chmodSync(path.join(app, "Contents/MacOS/Probe"), 0o755);
    writeFileSync(
      path.join(app, "Contents/Info.plist"),
      `<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>local.codex.agents</string><key>CFBundleExecutable</key><string>Probe</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>`,
    );
    const requirements = [];
    for (const version of [1, 2]) {
      const expectedLease = version === 1 ? "" : "existing lease\n";
      if (version === 2) writeFileSync(lease, expectedLease);
      const priorLeaseInode = version === 2 ? statSync(lease).ino : null;
      writeFileSync(
        path.join(scripts, "studio_signature_probe.py"),
        `value = ${version}\n`,
      );
      signApplication(app, identity);
      const leaseInode = statSync(lease).ino;
      assert.equal(readFileSync(lease, "utf8"), expectedLease);
      if (priorLeaseInode !== null) assert.equal(leaseInode, priorLeaseInode);
      const requirement = execFileSync("codesign", ["-d", "-r-", app], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      });
      requirements.push(requirement.trim());
      execFileSync(process.env.CODEX_AGENTS_PYTHON || "python3", [
        "-c",
        "import sys; sys.dont_write_bytecode = False; sys.pycache_prefix = None; sys.path.insert(0, sys.argv[1]); import studio_signature_probe; assert studio_signature_probe.value == int(sys.argv[2])",
        scripts,
        String(version),
      ]);
      assert.deepEqual(
        readdirSync(path.join(scripts, "__pycache__")),
        [],
        "imports cannot add unsigned cache files",
      );
      execFileSync(process.env.CODEX_AGENTS_PYTHON || "python3", [
        "-B",
        "-c",
        "import sys; from types import SimpleNamespace; sys.path.insert(0, sys.argv[1]); from codex_live_updates import LiveUpdates; manager = LiveUpdates(SimpleNamespace(root=sys.argv[3], closed=False), scripts=sys.argv[2]); manager.tick(); manager.tick(); assert manager.status()['status'] == 'idle', manager.status()",
        repositoryScripts,
        scripts,
        root,
      ]);
      assert.equal(statSync(lease).ino, leaseInode);
      assert.equal(readFileSync(lease, "utf8"), expectedLease);
      execFileSync("codesign", ["--verify", "--deep", "--strict", app]);
    }
    assert.match(requirements[0], /certificate leaf/);
    assert.equal(
      requirements[0],
      requirements[1],
      "resource updates preserve the application identity",
    );
    console.log(
      "PASS: stable certificate identity; Python imports and updater ticks preserve the resource seal and lease",
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}
