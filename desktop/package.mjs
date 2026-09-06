import { packager } from "@electron/packager";
import { cp, mkdir, mkdtemp, rm, access } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.dirname(fileURLToPath(import.meta.url));
await access(path.join(root, "../web/dist/index.html"));
const stage = await mkdtemp(path.join(tmpdir(), "codex-desktop-package-"));
try {
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
  const output = await packager({
    dir: root,
    name: "Codex Studio",
    executableName: "Codex Studio",
    appBundleId: "local.codex.agents",
    appCategoryType: "public.app-category.developer-tools",
    platform: "darwin",
    arch: "arm64",
    electronVersion: "44.2.0",
    out: path.join(root, "dist"),
    overwrite: true,
    asar: true,
    prune: true,
    extraResource: [resources],
    ignore: [
      /^\/dist($|\/)/,
      /^\/node_modules($|\/)/,
      /^\/test\.mjs$/,
      /^\/package\.mjs$/,
      /^\/README\.md$/,
    ],
  });
  console.log(output.join("\n"));
} finally {
  await rm(stage, { recursive: true, force: true });
}
