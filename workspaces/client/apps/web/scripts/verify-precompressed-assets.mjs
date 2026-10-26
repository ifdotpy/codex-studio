// Manual headless proof against a disposable real Rust server. Build the server
// and renderer first, then set CODEX_STUDIO_RUNTIME_SERVER to that exact binary.
import assert from "node:assert/strict";
import { once } from "node:events";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
import { browserExecutablePath } from "../tests/browser-executable.mjs";
import { startRustBackend } from "../tests/fixtures/rust-ui-fixture.mjs";

assert.ok(
  process.env.CODEX_STUDIO_RUNTIME_SERVER,
  "Select the branch's built server explicitly",
);
const dist = resolve(
  process.env.CODEX_STUDIO_WEB_DIST ||
    fileURLToPath(new URL("../dist", import.meta.url)),
);
process.env.CODEX_STUDIO_WEB_DIST = dist;
const root = await mkdtemp(join(tmpdir(), "studio-precompressed-browser-"));
const reportPath = process.argv[2] || join(root, "evidence.json");
const assets = (await readdir(join(dist, "assets"))).filter(
  (name) => !/\.(?:br|gz)$/.test(name),
);
const builtFiles = new Set(await readdir(dist, { recursive: true }));
const paths = [
  "/",
  "/studio-sw.js",
  "/studio-renderer.json",
  "/manifest.webmanifest",
  ...assets.map((name) => `/assets/${name}`),
];
const expected = new Map(
  await Promise.all(
    paths.map(async (path) => [
      path,
      await readFile(join(dist, path === "/" ? "index.html" : path)),
    ]),
  ),
);
let backend;
let browser;
try {
  await mkdir(join(root, "state"));
  backend = await startRustBackend(join(root, "state"));
  assert.notEqual(backend.port, 4620);
  browser = await chromium.launch({
    headless: true,
    executablePath: browserExecutablePath,
  });
  const runs = [];
  for (const mode of ["identity", "br"]) {
    const context = await browser.newContext({
      serviceWorkers: "block",
      ...(mode === "identity"
        ? { extraHTTPHeaders: { "Accept-Encoding": "identity" } }
        : {}),
    });
    try {
      const page = await context.newPage();
      const cdp = await context.newCDPSession(page);
      await cdp.send("Network.enable");
      await cdp.send("Network.setCacheDisabled", { cacheDisabled: true });
      const errors = [];
      page.on("pageerror", (error) => errors.push(String(error)));
      await page.goto(backend.origin, { waitUntil: "load" });
      await page
        .getByRole("button", { name: "New chat", exact: true })
        .waitFor({ timeout: 30_000 });
      const records = [];
      // Fetch every original once with the browser's real HTTP decoder. This
      // gives both modes exactly the same resource set, including lazy chunks.
      for (const path of paths) {
        const responsePromise = page.waitForResponse(
          (response) => new URL(response.url()).pathname === path,
        );
        await page.evaluate(async (path) => {
          const response = await fetch(path, { cache: "no-store" });
          if (!response.ok) throw new Error(`${path}: ${response.status}`);
          const bytes = await response.arrayBuffer();
          if (path.endsWith(".wasm")) await WebAssembly.compile(bytes);
        }, path);
        const response = await responsePromise;
        const headers = await response.allHeaders();
        assert.deepEqual(
          await response.body(),
          expected.get(path),
          `${mode} decoded ${path}`,
        );
        const encoding = headers["content-encoding"] || "identity";
        if (mode === "identity") assert.equal(encoding, "identity", path);
        else {
          const relative = path === "/" ? "index.html" : path.slice(1);
          const expectedEncoding = builtFiles.has(`${relative}.br`)
            ? "br"
            : builtFiles.has(`${relative}.gz`)
              ? "gzip"
              : "identity";
          assert.equal(encoding, expectedEncoding, path);
        }
        const sizes = await response.request().sizes();
        records.push({
          path,
          encoding,
          transferredBodyBytes: sizes.responseBodySize,
          transferredHeaderBytes: sizes.responseHeadersSize,
          decodedBytes: expected.get(path).length,
        });
      }
      assert.deepEqual(errors, [], "renderer loaded without page errors");
      runs.push({
        mode,
        appLoaded: true,
        assets: records.length,
        transferredBodyBytes: records.reduce(
          (sum, item) => sum + item.transferredBodyBytes,
          0,
        ),
        transferredBytes: records.reduce(
          (sum, item) =>
            sum + item.transferredBodyBytes + item.transferredHeaderBytes,
          0,
        ),
        records,
      });
    } finally {
      await context.close();
    }
  }
  assert.ok(runs[1].transferredBodyBytes < runs[0].transferredBodyBytes);
  await writeFile(
    reportPath,
    JSON.stringify(
      {
        server: process.env.CODEX_STUDIO_RUNTIME_SERVER,
        dist,
        origin: backend.origin,
        runs,
      },
      null,
      2,
    ),
  );
  console.log(
    JSON.stringify({
      reportPath,
      runs: runs.map(({ records: _records, ...summary }) => summary),
    }),
  );
} finally {
  try {
    await browser?.close();
  } finally {
    if (backend && backend.child.exitCode === null) {
      const exited = once(backend.child, "exit");
      backend.child.kill("SIGTERM");
      await exited;
    }
    await rm(join(root, "state"), { recursive: true, force: true });
  }
}
