import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

it("the Claude code receipt helper does not import browser networking", async () => {
  const result = await build({
    entryPoints: [
      fileURLToPath(new URL("./claudeCodeSubmission.ts", import.meta.url)),
    ],
    bundle: true,
    write: false,
    platform: "node",
    format: "esm",
    external: ["*.wasm?url"],
    metafile: true,
    logLevel: "silent",
  });
  const inputs = Object.keys(result.metafile!.inputs);
  expect(inputs.some((path) => path.endsWith("/apiErrors.ts"))).toBe(true);
  for (const path of inputs) {
    expect(path).not.toMatch(/(?:^|\/)src\/api\.ts$/);
    expect(path).not.toMatch(/openapi-fetch|servers\/transport|sync\/client/);
  }
});
