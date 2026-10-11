import { execFileSync } from "node:child_process";
import { mkdirSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";

// Sets the name that macOS shows in the menu bar, the About panel, the Dock and
// Finder. The bundle file name, executable, bundle identifier and Electron app
// name stay "Codex Studio": installs, recovery and the Keychain item use them.
export function setBundleDisplayName(application, name) {
  const info = path.join(application, "Contents/Info.plist");
  const fileName = path.basename(application, ".app");
  const current = execFileSync(
    "plutil",
    ["-extract", "CFBundleDisplayName", "raw", "-o", "-", info],
    { encoding: "utf8" },
  ).trim();
  // Finder and the Dock use the localized name only when this value equals the
  // bundle file name. Otherwise they silently show the file name.
  if (current !== fileName)
    throw new Error(
      `CFBundleDisplayName "${current}" must equal the bundle file name "${fileName}".`,
    );
  execFileSync("plutil", ["-replace", "CFBundleName", "-string", name, info]);
  execFileSync("plutil", [
    "-replace",
    "LSHasLocalizedDisplayName",
    "-bool",
    "YES",
    info,
  ]);
  const resources = path.join(application, "Contents/Resources");
  mkdirSync(path.join(resources, "en.lproj"), { recursive: true });
  const strings = `"CFBundleDisplayName" = ${JSON.stringify(name)};\n"CFBundleName" = ${JSON.stringify(name)};\n`;
  for (const entry of readdirSync(resources))
    if (entry.endsWith(".lproj"))
      writeFileSync(path.join(resources, entry, "InfoPlist.strings"), strings);
}
