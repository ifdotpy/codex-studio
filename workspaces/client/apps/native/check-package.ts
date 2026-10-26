import type { components } from "../web/src/generated/api.ts";
import { nativeRecoveryResourceFiles } from "./recovery-package.ts";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { access, readFile } from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import {
  runtimeBinaries,
  runtimeBinaryName,
} from "../desktop/runtime-binaries.ts";
import {
  missingPythonModules,
  pythonEntryPoints,
} from "../desktop/python-resources.ts";

// The desktop packager writes studio-build.json; this package check reads that
// same build document using the generated peer-update build owner.
function readBuildStamp(
  text: string,
): components["schemas"]["PeerUpdateBuild"] {
  return JSON.parse(text);
}

const application = process.argv[2];
if (!application) throw new Error("Supply the packaged .app path.");
const bundle = path.resolve(application);
const resources = path.join(bundle, "Contents/Resources");
const workspace = path.join(resources, "workspace");
const plist = (key: string) =>
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
  "workspace/web/dist/index.html",
  "helpers/studio-speech",
  ...nativeRecoveryResourceFiles,
  "studio-fs-events",
  "studio-linux-vm",
  ...pythonEntryPoints,
])
  await access(path.join(resources, file));
const helperManifest: unknown = JSON.parse(
  await readFile(path.join(resources, "helpers/package.json"), "utf8"),
);
assert.ok(
  helperManifest &&
    typeof helperManifest === "object" &&
    "type" in helperManifest,
);
assert.equal(helperManifest.type, "module");
assert.deepEqual(
  missingPythonModules(resources),
  [],
  "The packaged Python programs import modules that Contents/Resources does not supply.",
);
for (const { binary } of runtimeBinaries)
  await access(
    path.join(workspace, "bin", runtimeBinaryName(binary)),
    constants.X_OK,
  );
const stamp = readBuildStamp(
  await readFile(path.join(resources, "studio-build.json"), "utf8"),
);
assert.match(stamp.commit, /^[a-f0-9]{40}$/);
assert.ok(Number.isFinite(stamp.builtAt) && stamp.builtAt > 0);
execFileSync("codesign", ["--verify", "--deep", "--strict", bundle], {
  stdio: "inherit",
});
console.log(
  JSON.stringify({
    result: "PASS",
    bundle,
    executable,
    commit: stamp.commit,
    checks:
      "bundle resources, Python helper imports, Rust runtime executables, desktop icon, code signature",
  }),
);
