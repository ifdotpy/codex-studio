// @ts-check
// Native desktop entry point for the same guarded recovery configuration as Electron.
const path = require("node:path");
const helper = path.join(__dirname, "desktop-recovery", "recovery.cjs");
/** @type {typeof import("../desktop/recovery.cjs")} */
const { configureRecovery } = require(helper);

async function main() {
  const [executable, resources, enabled] = process.argv.slice(2);
  if (!enabled || !["true", "false"].includes(enabled))
    throw new Error("Invalid recovery setting.");
  if (
    path.resolve(resources || "") !==
    "/Applications/Codex Studio.app/Contents/Resources"
  )
    throw new Error("Recovery requires the installed application resources.");
  if (!executable || !resources)
    throw new Error("Recovery requires application paths.");
  const result = await configureRecovery({
    applicationPath: executable,
    resources: path.join(resources, "workspace"),
    supervisor: path.join(resources, "recover_backend.py"),
    port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
    enabled: enabled === "true",
  });
  process.stdout.write(JSON.stringify({ enabled: result.enabled }));
}
main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
