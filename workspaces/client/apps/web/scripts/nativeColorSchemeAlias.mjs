import { createHash } from "node:crypto";
import { readFileSync, realpathSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const mantine = dirname(fileURLToPath(import.meta.resolve("@mantine/core")));
/** Exact upstream fallback contract. No other dependency module receives this alias. */
export const NATIVE_MANTINE_COLOR_SCHEME = Object.freeze({
  version: "9.7.3",
  sourceHash:
    "2d43edcebfdf153040c53ab122c6e51e786b84d45771ec88a257740d7511e3b4",
  source: realpathSync(
    resolve(
      mantine,
      "core/MantineProvider/color-scheme-managers/local-storage-manager.mjs",
    ),
  ),
  packageJson: resolve(mantine, "../package.json"),
});
function verifyOwner() {
  const owner = NATIVE_MANTINE_COLOR_SCHEME;
  const version = JSON.parse(readFileSync(owner.packageJson, "utf8")).version;
  const hash = createHash("sha256")
    .update(readFileSync(owner.source))
    .digest("hex");
  if (version !== owner.version || hash !== owner.sourceHash)
    throw new Error(
      "The native color scheme alias needs an upstream contract review.",
    );
}
export const nativeColorSchemeAlias = {
  find: /(?:^|\/)local-storage-manager\.mjs(?:\?.*)?$/,
  replacement: "$&",
  async customResolver(importee, importer, options) {
    const resolved = await this.resolve(importee, importer, {
      ...options,
      skipSelf: true,
    });
    if (!resolved || resolved.external) return resolved;
    if (
      realpathSync(resolved.id.split("?")[0]) !==
      NATIVE_MANTINE_COLOR_SCHEME.source
    )
      return resolved;
    if (resolved.id.includes("?"))
      throw new Error("The native color scheme alias has an unknown query.");
    verifyOwner();
    return fileURLToPath(
      new URL("../src/native/colorSchemeManager.ts", import.meta.url),
    );
  },
};
