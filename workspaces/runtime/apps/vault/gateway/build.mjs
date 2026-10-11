// Build a gateway release (docs/vault.md, "Gateway"): the server bundle, the site and sync.py in
// dist/gateway.tgz. Prints its SHA-256.
import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, rmSync } from "node:fs";

const here = new URL(".", import.meta.url).pathname;
const out = new URL("../dist/gateway/", import.meta.url).pathname;
const site = new URL("../../../../client/apps/vault-web/", import.meta.url)
  .pathname;
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
execFileSync("pnpm", ["run", "build"], {
  cwd: site,
  stdio: ["ignore", "ignore", "inherit"],
});
await build({
  entryPoints: [`${here}main.mjs`],
  outfile: `${out}gateway.mjs`,
  bundle: true,
  platform: "node",
  // Debian 13 ships Node.js 20.
  target: "node20",
  format: "esm",
  // Some SDK modules still call require(); give the ESM bundle one.
  banner: {
    js: "import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);",
  },
  legalComments: "none",
  logLevel: "warning",
});
cpSync(`${site}dist`, `${out}site`, { recursive: true });
cpSync(`${here}sync.py`, `${out}sync.py`);
const archive = new URL("../dist/gateway.tgz", import.meta.url).pathname;
execFileSync("tar", ["-czf", archive, "-C", out, "."], {
  env: { ...process.env, COPYFILE_DISABLE: "1" },
});
console.log(createHash("sha256").update(readFileSync(archive)).digest("hex"));
