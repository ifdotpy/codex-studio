import type { AddressInfo } from "node:net";
import type { ShellManifest } from "../../scripts/build-manifests.ts";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { test, expect } from "../playwright.ts";

// The renderer build writes this manifest before this spec reads it.
function readShellManifest(worker: string): ShellManifest {
  const line = worker.split("\n", 1)[0];
  return JSON.parse(
    line!.slice("self.STUDIO_SHELL = ".length, -1),
  ) as ShellManifest;
}

test("built PWA caches its exact Rust WASM module for offline use", async ({
  page,
  context,
}) => {
  const dist = new URL("../../dist/", import.meta.url);
  const worker = await readFile(new URL("studio-sw.js", dist), "utf8");
  const manifest = readShellManifest(worker);
  const wasm = manifest.initial.filter((path) => path.endsWith(".wasm"));
  expect(wasm).toHaveLength(1);
  expect(manifest.allowed).toContain(wasm[0]);
  const server = createServer(async (request, response) => {
    const path = new URL(request.url!, "http://localhost").pathname;
    response.setHeader(
      "Content-Security-Policy",
      "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'",
    );
    if (path === "/check") {
      response.setHeader("Content-Type", "text/html");
      response.end("<!doctype html><title>Offline Rust check</title>");
      return;
    }
    if (!manifest.allowed.includes(path) && path !== "/studio-sw.js") {
      response.writeHead(404).end();
      return;
    }
    try {
      const bytes = await readFile(
        new URL(path === "/" ? "index.html" : path.slice(1), dist),
      );
      const type = path.endsWith(".wasm")
        ? "application/wasm"
        : path.endsWith(".js")
          ? "text/javascript"
          : path.endsWith(".css")
            ? "text/css"
            : "text/html";
      response.setHeader("Content-Type", type);
      response.end(bytes);
    } catch {
      response.writeHead(404).end();
    }
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await page.goto(
      `http://127.0.0.1:${(server.address() as AddressInfo).port}/check`,
    );
    await page.evaluate(async () => {
      await navigator.serviceWorker.register("/studio-sw.js");
      await navigator.serviceWorker.ready;
    });
    // The production worker intentionally does not claim an already-open page.
    await page.reload();
    expect(
      await page.evaluate(() => !!navigator.serviceWorker.controller),
    ).toBe(true);
    await context.setOffline(true);
    const exports = await page.evaluate(async (path) => {
      const response = await fetch(path);
      if (!response.ok) throw new Error(`Offline WASM: ${response.status}`);
      const module = await WebAssembly.compileStreaming(response);
      return WebAssembly.Module.exports(module).map((item) => item.name);
    }, wasm[0]);
    expect(exports).toContain("parse_markdown");
  } finally {
    await context.setOffline(false);
    await new Promise((resolve) => server.close(resolve));
  }
});
