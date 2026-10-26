import type { ViteDevServer } from "vite";
export { assert as fixtureAssert } from "../chat/assertions.ts";

/** Vite binds an ephemeral TCP port for every isolated sync fixture. */
export function fixtureOrigin(server: ViteDevServer, host = "127.0.0.1") {
  const address =
    server.httpServer!.address() as import("node:net").AddressInfo;
  return `http://${host}:${address.port}`;
}

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

export function draftPushDocument(
  document: import("../../src/sync/client.ts").SyncDocument,
) {
  const { _deleted, ...state } = document;
  return {
    ...state,
    deleted: _deleted === true,
  } satisfies import("../../src/generated/api.ts").components["schemas"]["DraftDocumentInput"];
}

export async function readDraftDocuments(origin: string) {
  const response = await fetch(
    `${origin}/api/sync/pull?scope=drafts&after=0&limit=100`,
  );
  const page: { documents: import("../../src/sync/client.ts").SyncDocument[] } =
    await response.json();
  return page.documents;
}

/** Existing pull fixtures omit the default generation and storage tombstone flag. */
export type FixturePullResponse = Omit<
  import("../playwright.ts").SyncPullResponse,
  "generation" | "documents"
> &
  Partial<Pick<import("../playwright.ts").SyncPullResponse, "generation">> & {
    documents: (
      | import("../playwright.ts").SyncEntity
      | import("../../src/sync/client.ts").SyncDocument
    )[];
  };

/** Some legacy stream baselines deliberately leave out per-resource versions. */
export type FixtureResourceChange = Omit<
  import("../playwright.ts").ResourceChangeEvent,
  "resourceVersions"
> &
  Partial<
    Pick<import("../playwright.ts").ResourceChangeEvent, "resourceVersions">
  >;
