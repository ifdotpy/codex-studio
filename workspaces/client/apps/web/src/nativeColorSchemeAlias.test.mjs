import { expect, it } from "vitest";
import { build } from "vite";
import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { inspectCapabilities } from "../scripts/nativeImportGraph.mjs";

it("replaces the actual pinned MantineProvider fallback module with the native owner", async () => {
  const aliases = await import("../scripts/nativeColorSchemeAlias.mjs").catch(
    () => ({ nativeColorSchemeAlias: undefined }),
  );
  const entry = fileURLToPath(
    new URL("./__native_mantine_witness__.ts", import.meta.url),
  );
  const output = await build({
    configFile: false,
    logLevel: "silent",
    resolve: {
      alias: aliases.nativeColorSchemeAlias
        ? [aliases.nativeColorSchemeAlias]
        : [],
    },
    plugins: [
      {
        name: "actual-mantine-caller",
        resolveId(id) {
          if (id === entry) return entry;
        },
        load(id) {
          if (id === entry)
            return 'import { MantineProvider } from "@mantine/core"; console.log(MantineProvider);';
        },
      },
    ],
    build: { write: false, minify: false, rolldownOptions: { input: entry } },
  });
  const findings = output.output
    .filter((item) => item.type === "chunk")
    .flatMap((item) =>
      inspectCapabilities(item.code, item.fileName, {
        ui: false,
        readonly: false,
      }),
    );
  expect(
    findings.filter((finding) => finding.capability === "browser-persistence"),
  ).toEqual([]);
  const modules = output.output
    .filter((item) => item.type === "chunk")
    .flatMap((item) => Object.keys(item.modules));
  expect(
    modules.some((id) => id.endsWith("native/colorSchemeManager.ts")),
  ).toBe(true);
  expect(
    modules.some((id) =>
      id.endsWith("color-scheme-managers/local-storage-manager.mjs"),
    ),
  ).toBe(false);
});
it("binds the exact upstream version, module bytes and default key to one alias owner", async () => {
  const { NATIVE_MANTINE_COLOR_SCHEME } =
    await import("../scripts/nativeColorSchemeAlias.mjs");
  const source = await readFile(NATIVE_MANTINE_COLOR_SCHEME.source, "utf8");
  const packageJson = JSON.parse(
    await readFile(NATIVE_MANTINE_COLOR_SCHEME.packageJson, "utf8"),
  );
  expect(packageJson.version).toBe(NATIVE_MANTINE_COLOR_SCHEME.version);
  expect(createHash("sha256").update(source).digest("hex")).toBe(
    NATIVE_MANTINE_COLOR_SCHEME.sourceHash,
  );
  const { NATIVE_COLOR_SCHEME_KEY } =
    await import("./native/colorSchemeManager");
  expect(source).toContain(`key = "${NATIVE_COLOR_SCHEME_KEY}"`);
});
