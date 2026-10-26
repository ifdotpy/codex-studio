import type { ViteDevServer } from "vite";
import { expect } from "../playwright.ts";

/** Vite binds an ephemeral TCP port for every isolated sync fixture. */
export function fixtureOrigin(server: ViteDevServer, host = "127.0.0.1") {
  const address = server.httpServer?.address();
  if (!address || typeof address === "string")
    throw new Error("Sync fixture TCP address unavailable");
  return `http://${host}:${address.port}`;
}

/** Preserve the existing Playwright assertion messages across sync specs. */
export const fixtureAssert = {
  equal: (actual: unknown, expected: unknown, message?: string) =>
    expect(actual, message).toBe(expected),
  notEqual: (actual: unknown, expected: unknown, message?: string) =>
    expect(actual, message).not.toBe(expected),
  deepEqual: (actual: unknown, expected: unknown, message?: string) =>
    expect(actual, message).toEqual(expected),
  ok: (actual: unknown, message?: string) =>
    expect(actual, message).toBeTruthy(),
  match: (actual: string, expected: RegExp, message?: string) =>
    expect(actual, message).toMatch(expected),
  doesNotMatch: (actual: string, expected: RegExp, message?: string) =>
    expect(actual, message).not.toMatch(expected),
  fail: (message: string): never => {
    throw new Error(message);
  },
};

/** Fixture JSON comes from the renderer under test; the generated route owns its shape. */
export function fixturePostBody<
  Path extends import("../../src/apiTypes.ts").ApiPostPath,
>(body: unknown): import("../../src/apiTypes.ts").PostBody<Path> {
  return body as import("../../src/apiTypes.ts").PostBody<Path>;
}

/** Fixture streams echo the selectors supplied by the typed renderer client. */
export function fixtureResources(value: string | null) {
  return JSON.parse(value || "[]") as import("../playwright.ts").ResourceRef[];
}
