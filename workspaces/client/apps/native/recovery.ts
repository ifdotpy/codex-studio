// Native desktop entry point for the same guarded recovery configuration as Electron.
import path from "node:path";
import { pathToFileURL } from "node:url";

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
main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});
