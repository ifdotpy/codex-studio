// Native desktop entry point for the same guarded recovery configuration as Electron.
import path from "node:path";
import { pathToFileURL } from "node:url";

async function main() {
  const [executable = "", resources = "", enabled = ""] = process.argv.slice(2);
  if (!["true", "false"].includes(enabled))
    throw new Error("Invalid recovery setting.");
  if (
    path.resolve(resources || "") !==
    "/Applications/Codex Studio.app/Contents/Resources"
  )
    throw new Error("Recovery requires the installed application resources.");
  const recovery: typeof import("../desktop/recovery.ts") = await import(
    pathToFileURL(
      path.join(import.meta.dirname, "desktop-recovery/recovery.ts"),
    ).href
  );
  const result = await recovery.configureRecovery({
    applicationPath: executable,
    resources: path.join(resources, "workspace"),
    supervisor: path.join(resources, "recover_backend.py"),
    port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
    enabled: enabled === "true",
  });
  process.stdout.write(JSON.stringify({ enabled: result.enabled }));
}
main().catch((error: Error) => {
  console.error(error.message);
  process.exitCode = 1;
});
