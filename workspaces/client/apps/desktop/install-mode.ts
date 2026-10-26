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
    return (
      JSON.parse(
        fs.readFileSync(
          path.join(resourcesPath ?? "", "workspace/studio-install.json"),
          "utf8",
        ),
      ).mode === "ui-only"
    );
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code === "ENOENT") return false;
    throw error;
  }
}
export { uiOnlyInstallation };
