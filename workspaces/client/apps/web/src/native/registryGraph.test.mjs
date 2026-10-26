import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";
import { checkNativeImportGraph } from "../../scripts/nativeImportGraph.mjs";
import { nativePlatformAliases } from "../../scripts/nativePlatformAliases.mjs";

it.each([
  ["registry", "src/native/registry.ts"],
  ["serverAliases", "src/native/serverAliases.ts"],
])(
  "the actual %s import uses native ownership without browser defaults",
  async (name, target) => {
    const report = await checkNativeImportGraph({
      root: fileURLToPath(new URL("../../", import.meta.url)),
      entry: fileURLToPath(new URL(`../servers/${name}.ts`, import.meta.url)),
      resolve: { alias: nativePlatformAliases },
    });
    expect(report.findings).toEqual([]);
    expect(report.modules).toContain(target);
    expect(report.modules).not.toContain("src/servers/registry.ts");
    expect(report.modules).not.toContain("src/servers/serverAliases.ts");
  },
);
