import { readFileSync } from "node:fs";
import { readFile, readdir, rm, writeFile } from "node:fs/promises";
import { extname, join, resolve } from "node:path";
import { promisify } from "node:util";
import { brotliCompress, constants, gzip } from "node:zlib";
import type { Plugin } from "vite";

// The server owns this extension contract; both build and serving consume it.
const compressibleExtensions = new Set(
  readFileSync(
    new URL(
      "../../../../runtime/apps/runtime-server/src/web_asset_extensions.txt",
      import.meta.url,
    ),
    "utf8",
  )
    .trim()
    .split(/\s+/),
);
const brotli = promisify(brotliCompress);
const gzipAsync = promisify(gzip);
const BROTLI_QUALITY = 11;
const GZIP_LEVEL = 9;
// Bound simultaneous file buffers and zlib tasks; never use a worker per asset.
const COMPRESSION_WORKERS = 4;

async function compressDirectory(directory: string): Promise<void> {
  const files: string[] = [];
  async function visit(path: string): Promise<void> {
    for (const entry of await readdir(path, { withFileTypes: true })) {
      const file = join(path, entry.name);
      if (entry.isDirectory()) await visit(file);
      else if (
        entry.isFile() &&
        compressibleExtensions.has(extname(file).slice(1))
      )
        files.push(file);
    }
  }
  await visit(directory);
  const pending = files.values();
  await Promise.all(
    Array.from({ length: COMPRESSION_WORKERS }, async () => {
      for (const file of pending) {
        const original = await readFile(file);
        for (const [suffix, encode] of [
          [
            "br",
            () =>
              brotli(original, {
                params: { [constants.BROTLI_PARAM_QUALITY]: BROTLI_QUALITY },
              }),
          ],
          ["gz", () => gzipAsync(original, { level: GZIP_LEVEL })],
        ] as const) {
          const compressed = await encode();
          const sibling = `${file}.${suffix}`;
          if (compressed.length < original.length)
            await writeFile(sibling, compressed);
          else await rm(sibling, { force: true });
        }
      }
    }),
  );
}

export function precompressAssets(): Plugin {
  let outputDirectory: string;
  let write: boolean;
  return {
    name: "studio-precompressed-assets",
    apply: "build",
    enforce: "post",
    configResolved(config) {
      outputDirectory = resolve(config.root, config.build.outDir);
      write = config.build.write;
    },
    // Run after the offline shell has finalized HTML, worker, and build hashes,
    // and after Vite has copied public files. Siblings never enter the bundle.
    async closeBundle() {
      if (write) await compressDirectory(outputDirectory);
    },
  };
}
