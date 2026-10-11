import assert from "node:assert/strict";
import { test } from "vitest";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { setBundleDisplayName } from "./bundle-name.mjs";

const read = (file) =>
  JSON.parse(
    execFileSync("plutil", ["-convert", "json", "-o", "-", file], {
      encoding: "utf8",
    }),
  );
function bundle(fileName, displayName = fileName) {
  const root = mkdtempSync(path.join(tmpdir(), "studio-bundle-name-"));
  const application = path.join(root, `${fileName}.app`);
  for (const locale of ["en", "ru"])
    mkdirSync(path.join(application, `Contents/Resources/${locale}.lproj`), {
      recursive: true,
    });
  writeFileSync(
    path.join(application, "Contents/Info.plist"),
    `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleDisplayName</key><string>${displayName}</string>
<key>CFBundleExecutable</key><string>Codex Studio</string>
<key>CFBundleIdentifier</key><string>local.codex.agents</string>
<key>CFBundleName</key><string>Codex Studio</string>
</dict></plist>
`,
  );
  return { root, application };
}

test.skipIf(process.platform !== "darwin")(
  "the bundle shows LAYR Studio and keeps its identities",
  () => {
    const { root, application } = bundle("Codex Studio");
    try {
      setBundleDisplayName(application, "LAYR Studio");
      assert.deepEqual(read(path.join(application, "Contents/Info.plist")), {
        CFBundleDisplayName: "Codex Studio",
        CFBundleExecutable: "Codex Studio",
        CFBundleIdentifier: "local.codex.agents",
        CFBundleName: "LAYR Studio",
        LSHasLocalizedDisplayName: true,
      });
      for (const locale of ["en", "ru"])
        assert.deepEqual(
          read(
            path.join(
              application,
              `Contents/Resources/${locale}.lproj/InfoPlist.strings`,
            ),
          ),
          { CFBundleDisplayName: "LAYR Studio", CFBundleName: "LAYR Studio" },
        );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  },
);

test.skipIf(process.platform !== "darwin")(
  "a display name that differs from the file name is refused",
  () => {
    const { root, application } = bundle("Codex Studio", "Other");
    try {
      assert.throws(
        () => setBundleDisplayName(application, "LAYR Studio"),
        /must equal the bundle file name "Codex Studio"/,
      );
      assert.equal(
        read(path.join(application, "Contents/Info.plist")).CFBundleName,
        "Codex Studio",
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  },
);
