// Real macOS signatures and Python imports, with an isolated probe bundle.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  copyFileSync,
  chmodSync,
  writeFileSync,
  readdirSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { signApplication, signingIdentity } from "./signing.mjs";

const identity = signingIdentity();
const root = mkdtempSync(path.join(tmpdir(), "studio-signature-"));
const app = path.join(root, "Probe.app");
const scripts = path.join(app, "Contents/Resources/workspace/scripts");
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
    writeFileSync(
      path.join(scripts, "studio_signature_probe.py"),
      `value = ${version}\n`,
    );
    signApplication(app, identity);
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
    execFileSync("codesign", ["--verify", "--deep", "--strict", app]);
  }
  assert.match(requirements[0], /certificate leaf/);
  assert.equal(
    requirements[0],
    requirements[1],
    "resource updates preserve the application identity",
  );
  console.log(
    "PASS: stable certificate identity; Python imports preserve the resource seal",
  );
} finally {
  rmSync(root, { recursive: true, force: true });
}
