// Bundle the vault function into one ESM file and zip it (dist/lambda.zip).
import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync } from "node:fs";

const out = new URL("../dist/lambda/", import.meta.url).pathname;
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
await build({
  entryPoints: [new URL("./handler.mjs", import.meta.url).pathname],
  outfile: `${out}index.mjs`,
  bundle: true,
  platform: "node",
  target: "node22",
  format: "esm",
  // Some SDK modules still call require(); give the ESM bundle one.
  banner: {
    js: "import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);",
  },
  legalComments: "none",
  logLevel: "warning",
});
rmSync(`${out}../lambda.zip`, { force: true });
execFileSync("zip", [
  "-q",
  "-X",
  "-j",
  `${out}../lambda.zip`,
  `${out}index.mjs`,
]);
console.log(`${out}../lambda.zip`);
