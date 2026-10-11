// The zstd decoder is loaded by URL; copy it next to the site instead of relying on bundling.
import { copyFileSync, mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

const require = createRequire(
  new URL("../../../../runtime/apps/vault/package.json", import.meta.url),
);
const entry = require.resolve("@bokuweb/zstd-wasm");
const root = entry.slice(0, entry.lastIndexOf("/dist/"));
// public/ holds only this generated file, so a clean checkout has no such folder yet.
mkdirSync(new URL("../public/", import.meta.url), { recursive: true });
copyFileSync(
  join(root, "dist/web/zstd.wasm"),
  new URL("../public/zstd.wasm", import.meta.url),
);
