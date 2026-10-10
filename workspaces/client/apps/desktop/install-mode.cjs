const fs = require("node:fs");
const path = require("node:path");
function uiOnlyInstallation({
  argv = process.argv,
  env = process.env,
  packaged = false,
  resourcesPath = process.resourcesPath,
} = {}) {
  if (argv.includes("--ui-only") || env.CODEX_DESKTOP_UI_ONLY === "1")
    return true;
  if (!packaged) return false;
  try {
    return (
      JSON.parse(
        fs.readFileSync(
          path.join(resourcesPath, "workspace/studio-install.json"),
          "utf8",
        ),
      ).mode === "ui-only"
    );
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}
module.exports = { uiOnlyInstallation };
