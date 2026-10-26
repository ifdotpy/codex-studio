import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { access, readFile } from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import {
  runtimeBinaries,
  runtimeBinaryName,
} from "../desktop/runtime-binaries.mjs";

const application = process.argv[2];
if (!application) throw new Error("Supply the packaged .app path.");
const bundle = path.resolve(application);
const resources = path.join(bundle, "Contents/Resources");
const workspace = path.join(resources, "workspace");
const plist = (key) =>
  execFileSync(
    "plutil",
    [
      "-extract",
      key,
      "raw",
      "-o",
      "-",
      path.join(bundle, "Contents/Info.plist"),
    ],
    { encoding: "utf8" },
  ).trim();
assert.equal(plist("CFBundleIdentifier"), "local.codex.agents");
for (const key of [
  "NSMicrophoneUsageDescription",
  "NSSpeechRecognitionUsageDescription",
])
  assert.ok(plist(key).length > 0, `Missing privacy description: ${key}`);
const executable = plist("CFBundleExecutable");
assert.equal(path.basename(executable), executable);
await access(path.join(bundle, "Contents/MacOS", executable));
const iconName = plist("CFBundleIconFile");
assert.equal(path.basename(iconName), iconName);
assert.deepEqual(
  await readFile(
    path.join(
      resources,
      iconName.endsWith(".icns") ? iconName : `${iconName}.icns`,
    ),
  ),
  await readFile(
    new URL("../desktop/assets/codex-studio.icns", import.meta.url),
  ),
  "The native application must use the canonical desktop icon.",
);
for (const file of [
  "workspace/scripts/codex-canvas",
  "workspace/scripts/codex_process_supervisor.py",
  "workspace/bin/studio_operations_native.abi3.so",
  "workspace/web/dist/index.html",
  "workspace/requirements.txt",
  "helpers/studio-speech",
  "helpers/recovery.cjs",
  "helpers/desktop-recovery/recovery.cjs",
  "helpers/desktop-recovery/backend.cjs",
  "helpers/desktop-recovery/restart-environment.cjs",
  "studio-fs-events",
  "studio-linux-vm",
  "recover_backend.py",
  "install_update.py",
])
  await access(path.join(resources, file));
for (const { binary } of runtimeBinaries)
  await access(
    path.join(workspace, "bin", runtimeBinaryName(binary)),
    constants.X_OK,
  );
const stamp = JSON.parse(
  await readFile(path.join(resources, "studio-build.json"), "utf8"),
);
assert.match(stamp.commit, /^[a-f0-9]{40}$/);
assert.ok(Number.isFinite(stamp.builtAt) && stamp.builtAt > 0);
execFileSync("codesign", ["--verify", "--deep", "--strict", bundle], {
  stdio: "inherit",
});
if (!process.env.CODEX_AGENTS_PYTHON)
  throw new Error(
    "Set CODEX_AGENTS_PYTHON to the prepared runtime interpreter.",
  );
execFileSync(
  process.env.CODEX_AGENTS_PYTHON,
  [
    "-B",
    "-c",
    "import sys; sys.path.insert(0, sys.argv[1]); import codex_operations_adapter; import fastapi, httpx, pydantic, uvicorn",
    path.join(workspace, "scripts"),
  ],
  { stdio: "inherit" },
);
console.log(
  JSON.stringify({
    result: "PASS",
    bundle,
    executable,
    commit: stamp.commit,
    checks:
      "bundle resources, Rust runtime executables, desktop icon, native Python adapter import, code signature",
  }),
);
