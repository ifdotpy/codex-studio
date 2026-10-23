import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

// theme.ts owns filled primary backgrounds and their scheme-aware contrast.
// Audit every JSX consumer, including rarely opened creation/sign-in dialogs.
test("primary actions do not override the theme with indigo or a white foreground", () => {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const invalid = readdirSync(root, { recursive: true, encoding: "utf8" })
    .filter((file) => file.endsWith(".tsx"))
    .filter((file) => {
      const source = readFileSync(
        new URL(`../${file}`, import.meta.url),
        "utf8",
      );
      return /color=(?:"indigo"|\{[^}]*"indigo")|\bc="white"/.test(source);
    });
  expect(invalid).toEqual([]);
});
