import { cp, mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

// Runtime imports of the desktop recovery owner; type-only imports need no resources.
const desktopRecoveryFiles = [
  "recovery.ts",
  "backend.ts",
  "restart-environment.ts",
] as const;
export const nativeRecoveryResourceFiles = [
  "helpers/package.json",
  "helpers/recovery.ts",
  ...desktopRecoveryFiles.map((name) => `helpers/desktop-recovery/${name}`),
];

export async function packageRecoveryHelpers(resources: string) {
  const helpers = path.join(resources, "helpers");
  const desktop = path.join(helpers, "desktop-recovery");
  await mkdir(desktop, { recursive: true });
  await writeFile(
    path.join(helpers, "package.json"),
    JSON.stringify({ type: "module" }),
  );
  await cp(
    path.join(import.meta.dirname, "recovery.ts"),
    path.join(helpers, "recovery.ts"),
  );
  for (const name of desktopRecoveryFiles)
    await cp(
      path.join(import.meta.dirname, "../desktop", name),
      path.join(desktop, name),
    );
}
