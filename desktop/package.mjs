import { execFileSync } from "node:child_process";
import { packager } from "@electron/packager";
import { cp, mkdir, mkdtemp, rm, access } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.dirname(fileURLToPath(import.meta.url));
await access(path.join(root, "../web/dist/index.html"));
const stage = await mkdtemp(path.join(tmpdir(), "codex-desktop-package-"));
try {
  const speech = path.join(stage, "studio-speech");
  execFileSync("xcrun", [
    "swiftc",
    path.join(root, "native/speech.swift"),
    "-O",
    "-o",
    speech,
    "-Xlinker",
    "-sectcreate",
    "-Xlinker",
    "__TEXT",
    "-Xlinker",
    "__info_plist",
    "-Xlinker",
    path.join(root, "native/speech-info.plist"),
  ]);
  execFileSync("codesign", ["--force", "--sign", "-", speech]);
  const resources = path.join(stage, "workspace");
  await mkdir(path.join(resources, "web"), { recursive: true });
  await cp(path.join(root, "../scripts"), path.join(resources, "scripts"), {
    recursive: true,
    filter: (source) =>
      !source.includes("__pycache__") && !source.endsWith(".pyc"),
  });
  await cp(path.join(root, "../web/dist"), path.join(resources, "web/dist"), {
    recursive: true,
  });
  await cp(
    path.join(root, "../.agents/skills"),
    path.join(resources, ".agents/skills"),
    { recursive: true },
  );
  for (const document of ["README.md", "CLI.md", "ORCHESTRATION.md"]) {
    await cp(path.join(root, "..", document), path.join(resources, document));
  }
  const output = await packager({
    dir: root,
    name: "Codex Studio",
    executableName: "Codex Studio",
    icon: path.join(root, "assets/codex-studio.icns"),
    appBundleId: "local.codex.agents",
    appCategoryType: "public.app-category.developer-tools",
    platform: "darwin",
    arch: "arm64",
    electronVersion: "44.2.0",
    out: path.join(root, "dist"),
    overwrite: true,
    asar: true,
    prune: true,
    extraResource: [resources, speech, path.join(root, "recover_backend.py")],
    extendInfo: {
      NSMicrophoneUsageDescription:
        "Record dictation that you can review and convert to text.",
      NSSpeechRecognitionUsageDescription:
        "Convert your saved dictation to text on this Mac.",
    },
    ignore: [
      /^\/assets($|\/)/,
      /^\/dist($|\/)/,
      /^\/node_modules($|\/)/,
      /^\/test\.mjs$/,
      /^\/(?:backend-test|recovery-test|recovery-renderer-test|window-state-test|package-test)\.(?:mjs|cjs|py)$/,
      /^\/recover_backend\.py$/,
      /^\/__pycache__($|\/)/,
      /^\/package\.mjs$/,
      /^\/README\.md$/,
    ],
  });
  console.log(output.join("\n"));
} finally {
  await rm(stage, { recursive: true, force: true });
}
