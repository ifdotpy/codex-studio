import * as fs from "node:fs";
import * as path from "node:path";
function uiOnlyInstallation({
  argv = process.argv,
  env = process.env,
  packaged = false,
  resourcesPath = process.resourcesPath,
}: {
  argv?: string[];
  env?: NodeJS.ProcessEnv;
  packaged?: boolean;
  resourcesPath?: string;
} = {}) {
  if (argv.includes("--ui-only") || env.CODEX_DESKTOP_UI_ONLY === "1")
    return true;
  if (!packaged) return false;
  try {
    // The packager writes this install-mode marker in the application resources.
    const marker: { mode?: unknown } = JSON.parse(
      fs.readFileSync(
        path.join(resourcesPath, "workspace/studio-install.json"),
        "utf8",
      ),
    );
    return marker.mode === "ui-only";
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error as NodeJS.ErrnoException;
  }
}
export { uiOnlyInstallation };
