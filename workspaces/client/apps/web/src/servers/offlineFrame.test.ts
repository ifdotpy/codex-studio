/// <reference lib="webworker" />
import assert from "node:assert/strict";
import { expect, test } from "vitest";
import vm from "node:vm";
import { readFile } from "node:fs/promises";
test("offline server navigation has frame CSP and cannot replace the root shell policy", async () => {
  const handlers: Record<
    string,
    (event: {
      request: Pick<Request, "method" | "mode" | "url">;
      respondWith(value: Promise<Response>): void;
      waitUntil(): void;
    }) => void
  > = {};
  const root = new Response('<meta name="studio-build" content="test" />', {
    headers: {
      "Content-Type": "text/html",
      "Content-Security-Policy":
        "default-src 'self'; frame-ancestors 'none'; base-uri 'none'",
    },
  });
  const cache = {
    match: async (key: string) =>
      key === "/"
        ? root.clone()
        : key === "/__studio_shell_manifest__"
          ? Response.json({ build: "test", initial: ["/"], allowed: ["/"] })
          : undefined,
  };
  vm.runInNewContext(
    await readFile(
      new URL("../../public/studio-sw.js", import.meta.url),
      "utf8",
    ),
    {
      self: {
        STUDIO_SHELL: { build: "test", initial: ["/"], allowed: ["/"] },
        location: { origin: "http://localhost" },
        addEventListener: (name: string, handler: (typeof handlers)[string]) =>
          (handlers[name] = handler),
      },
      caches: {
        keys: async () => ["studio-shell-v1-test"],
        open: async () => cache,
      },
      fetch: async () => {
        throw new TypeError("Offline");
      },
      Headers,
      Response,
      Request,
      URL,
      setTimeout,
      clearTimeout,
    },
  );
  let result: Promise<Response> | undefined;
  assert.ok(handlers.fetch);
  handlers.fetch({
    request: {
      method: "GET",
      mode: "navigate",
      url: "http://localhost/?studio-server=remote",
    },
    respondWith: (value: Promise<Response>) => (result = value),
    waitUntil() {},
  });
  const response = await result;
  assert.ok(response);
  assert.ok(response);
  expect(response.headers.get("Content-Security-Policy")).toContain(
    "frame-ancestors 'self'",
  );
  expect(root.headers.get("Content-Security-Policy")).toContain(
    "frame-ancestors 'none'",
  );
});
test("a network server-view response never overwrites the root cached HTML", async () => {
  const handlers: Record<
    string,
    (event: {
      request: Pick<Request, "method" | "mode" | "url">;
      respondWith(value: Promise<Response>): void;
      waitUntil(): void;
    }) => void
  > = {};
  const writes: string[] = [];
  const html = '<meta name="studio-build" content="test" />';
  const response = () =>
    new Response(html, {
      headers: {
        "Content-Type": "text/html",
        "Content-Security-Policy": "frame-ancestors 'self'",
      },
    });
  vm.runInNewContext(
    await readFile(
      new URL("../../public/studio-sw.js", import.meta.url),
      "utf8",
    ),
    {
      self: {
        STUDIO_SHELL: { build: "test", initial: ["/"], allowed: ["/"] },
        location: { origin: "http://localhost" },
        addEventListener: (name: string, handler: (typeof handlers)[string]) =>
          (handlers[name] = handler),
      },
      caches: {
        keys: async () => [],
        open: async () => ({ put: async (key: string) => writes.push(key) }),
      },
      fetch: async () => response(),
      Response,
      Request,
      Headers,
      URL,
      setTimeout,
      clearTimeout,
    },
  );
  let result: Promise<Response> | undefined;
  assert.ok(handlers.fetch);
  handlers.fetch({
    request: {
      method: "GET",
      mode: "navigate",
      url: "http://localhost/?studio-server=remote",
    },
    respondWith: (value: Promise<Response>) => (result = value),
    waitUntil() {},
  });
  await result;
  expect(writes).toEqual([]);
});
