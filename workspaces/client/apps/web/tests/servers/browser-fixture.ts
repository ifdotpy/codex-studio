import assert from "node:assert/strict";
import type { BrowserContext, Page } from "playwright";
import type { ViteDevServer } from "vite";
import type { ApiPostPath, PostBody } from "../../src/apiTypes.ts";
import type {
  MultiServerFixtureOptions,
  fixture,
} from "./multi-server-fixture.ts";

export type ServerFixture = Awaited<ReturnType<typeof fixture>>;
export type FixtureAccount = NonNullable<
  MultiServerFixtureOptions["accounts"]
>[number];

/** Requests come from the renderer under test, typed by the generated contract. */
export function fixturePostBody<Path extends ApiPostPath>(
  body: unknown,
): PostBody<Path> {
  return body as PostBody<Path>;
}

export function fixtureOrigin(server: ViteDevServer): string {
  const address = server.httpServer?.address();
  assert.ok(
    address && typeof address !== "string",
    "Fixture must listen on a TCP port",
  );
  return `http://127.0.0.1:${address.port}`;
}

export async function settings(page: Page) {
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  await page.getByRole("tab", { name: "Servers", exact: true }).click();
  return page.getByRole("dialog", { name: "Studio settings", exact: true });
}

export async function remoteNetwork(
  context: BrowserContext,
  remote: ServerFixture,
) {
  await context.addInitScript(
    ({ origin, destination }) => {
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const request = new Request(input, init);
        const url = new URL(request.url);
        if (url.origin !== origin) return nativeFetch(request);
        const body = await request.clone().arrayBuffer();
        return nativeFetch(destination + url.pathname + url.search, {
          method: request.method,
          headers: request.headers,
          ...(body.byteLength ? { body } : {}),
          signal: request.signal,
          redirect: "error",
          credentials: "omit",
        });
      };
    },
    { origin: remote.invitation.origin, destination: remote.origin },
  );
}

/** A response barrier is resolved by the scenario's explicit release step. */
export function fixtureBarrier() {
  const barrier: { resolve?: () => void } = {};
  const promise = new Promise<void>((resolve) => {
    barrier.resolve = resolve;
  });
  assert.ok(barrier.resolve);
  return { promise, resolve: barrier.resolve };
}

/** Preserve the fixture's failure on a missing row without non-null assertions. */
export function requiredFixtureValue<T>(value: T): NonNullable<T> {
  if (value === null || value === undefined)
    throw new Error("Missing fixture value");
  return value;
}
