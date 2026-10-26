import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
// Electron and Tauri share the established desktop artwork as their source.
const source = path.resolve(root, "../desktop/assets/codex-studio.icns");
const output = path.join(root, "src-tauri/icons");
const temporary = mkdtempSync(path.join(tmpdir(), "studio-icons-"));
try {
  const png = path.join(temporary, "icon.png");
  execFileSync("sips", ["-s", "format", "png", source, "--out", png], {
    stdio: "inherit",
  });
  execFileSync("pnpm", ["exec", "tauri", "icon", png, "--output", output], {
    cwd: root,
    stdio: "inherit",
  });
  // Keep the original macOS resolutions rather than re-encoding the ICNS.
  copyFileSync(source, path.join(output, "icon.icns"));
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
