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
import { test } from "vitest";
import { build } from "vite";
import {
  brotliCompressSync,
  brotliDecompressSync,
  constants,
  gunzipSync,
  gzipSync,
} from "node:zlib";

const web = new URL("../", import.meta.url);

test("the production Vite build compresses final outputs without changing shell URLs", async () => {
  const root = await mkdtemp(join(tmpdir(), "studio-precompression-"));
  try {
    await mkdir(join(root, "public"));
    await writeFile(
      join(root, "index.html"),
      '<html><head></head><body><script type="module" src="/entry.ts"></script></body></html>',
    );
    await writeFile(
      join(root, "entry.ts"),
      await readFile(
        new URL("./precompressedAssets.fixture.ts", import.meta.url),
        "utf8",
      ),
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
      ] as const) {
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
    const manifest = worker.match(/^self\.STUDIO_SHELL = (.*);/)?.[1];
    assert.ok(manifest);
    const shell: { initial: string[]; allowed: string[]; build: string } =
      JSON.parse(manifest);
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
