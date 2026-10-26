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

type NamedFields<T> = {
  [
    K in keyof T as string extends K ? never : number extends K ? never : K
  ]: T[K];
};
type UnionKeys<T> = T extends unknown ? keyof NamedFields<T> : never;
type UnionField<T, K extends PropertyKey> = T extends unknown
  ? K extends keyof NamedFields<T>
    ? NamedFields<T>[K]
    : never
  : never;
type RequestView<T, U = T> = T extends unknown
  ? T & {
      [K in Exclude<UnionKeys<U>, keyof T>]?: UnionField<U, K>;
    }
  : never;
export type FixtureRequestFields<Path extends ApiPostPath = ApiPostPath> = {
  [K in UnionKeys<PostBody<Path>>]?: UnionField<PostBody<Path>, K>;
};

/** Requests come from the renderer under test, typed by the generated contract. */
export function fixturePostBody<
  Path extends ApiPostPath,
  Body extends PostBody<Path> = PostBody<Path>,
>(body: unknown): RequestView<Body> {
  return body as RequestView<Body>;
}

export function fixtureOrigin(server: ViteDevServer): string {
  const address =
    server.httpServer!.address() as import("node:net").AddressInfo;
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

  return { promise, resolve: barrier.resolve! };
}
