import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { test } from "node:test";
import {
  brotliCompressSync,
  brotliDecompressSync,
  constants,
  gunzipSync,
  gzipSync,
} from "node:zlib";

const web = new URL("../../../../client/apps/web/", import.meta.url);
const require = createRequire(new URL("package.json", web));
const { build } = await import(require.resolve("vite"));

test("the production Vite build compresses final outputs without changing shell URLs", async () => {
  const root = await mkdtemp(join(tmpdir(), "studio-precompression-"));
  try {
    await mkdir(join(root, "public"));
    await writeFile(
      join(root, "index.html"),
      '<html><head></head><body><script type="module" src="/entry.js"></script></body></html>',
    );
    await writeFile(
      join(root, "entry.js"),
      'import "./style.css"; console.log("' +
        "compress me ".repeat(300) +
        '");',
    );
    await writeFile(
      join(root, "style.css"),
      ':root { --message: "' + "style ".repeat(300) + '"; }',
    );
    for (const extension of [
      "js",
      "css",
      "wasm",
      "svg",
      "html",
      "json",
      "webmanifest",
    ]) {
      await writeFile(
        join(root, "public", `large.${extension}`),
        "compressible fixture ".repeat(300),
      );
    }
    await writeFile(join(root, "public", "tiny.js"), "x");
    await writeFile(
      join(root, "public", "image.png"),
      "uncompressed fixture ".repeat(300),
    );
    await build({
      configFile: fileURLToPath(new URL("vite.config.ts", web)),
      root,
      logLevel: "silent",
      build: { outDir: join(root, "dist") },
    });
    const dist = join(root, "dist");
    const files = await readdir(dist, { recursive: true });
    for (const name of files.filter(
      (name) =>
        /\.(js|css|wasm|svg|html|json|webmanifest)$/.test(name) &&
        name !== "tiny.js",
    )) {
      const original = await readFile(join(dist, name));
      for (const [suffix, decode] of [
        ["br", brotliDecompressSync],
        ["gz", gunzipSync],
      ]) {
        const expected =
          suffix === "br"
            ? brotliCompressSync(original, {
                params: { [constants.BROTLI_PARAM_QUALITY]: 11 },
              })
            : gzipSync(original, { level: 9 });
        if (expected.length >= original.length) {
          assert.ok(
            !files.includes(`${name}.${suffix}`),
            `${name}.${suffix} must save bytes`,
          );
          continue;
        }
        const encoded = await readFile(join(dist, `${name}.${suffix}`));
        assert.deepEqual(
          encoded,
          expected,
          `${name}.${suffix} uses the requested compression level`,
        );
        assert.ok(
          encoded.length < original.length,
          `${name}.${suffix} saves bytes`,
        );
        assert.deepEqual(
          decode(encoded),
          original,
          `${name}.${suffix} contains the final output`,
        );
      }
    }
    assert.ok(
      !files.some((name) => /(?:tiny\.js|image\.png)\.(?:br|gz)$/.test(name)),
    );
    assert.ok(!files.some((name) => /\.(?:br|gz)\.(?:br|gz)$/.test(name)));
    const worker = await readFile(join(dist, "studio-sw.js"), "utf8");
    const shell = JSON.parse(worker.match(/^self\.STUDIO_SHELL = (.*);/)[1]);
    assert.ok(shell.initial.length > 0);
    assert.ok(
      [...shell.initial, ...shell.allowed].every(
        (url) => !/\.(br|gz)$/.test(url),
      ),
    );
    assert.equal(
      JSON.parse(await readFile(join(dist, "studio-renderer.json"), "utf8"))
        .build,
      shell.build,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
