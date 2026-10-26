import type { Server } from "node:http";
import type { ViteDevServer } from "vite";
import type { ApiPostPath, JsonInputOf, PostBody } from "../../src/apiTypes.ts";
import type { Account, AccountsState } from "../../src/components/Accounts.tsx";
import type { Agent } from "../../src/types.ts";
import type {
  EntityFixtureState,
  TestRuntimeView,
  handleEntitySyncFixtureRequest,
} from "../playwright.ts";

export function fixtureUrl(server: Server | ViteDevServer): string {
  const address = (
    "httpServer" in server ? server.httpServer! : server
  ).address() as import("node:net").AddressInfo;
  return `http://127.0.0.1:${address.port}`;
}
export function fixtureAccount(
  value: Partial<Account> & Pick<Account, "id" | "label">,
): Account {
  return Object.assign(
    {
      id: value.id,
      label: value.label,
      home: `/fixture/${value.id}`,
      provider: "codex",
      source: "Codex",
      status: "ready",
    } satisfies Account,
    value,
  );
}
export function fixtureAccounts(
  value: Partial<AccountsState> &
    Pick<AccountsState, "accounts" | "defaultAccountKey">,
): AccountsState {
  return {
    archivedAccounts: [],
    logins: [],
    defaultAccountGroups: [],
    supportsDelete: true,
    supportsDisconnect: true,
    ...value,
  };
}
export function fixtureLead(
  id: string,
  name: string,
  accountKey: string,
  empty: boolean,
  cwd: string,
): Agent {
  return {
    id,
    rootId: id,
    name,
    accountKey,
    empty,
    threadId: empty ? null : `thread-${id}`,
    isLead: true,
    source: "managed",
    status: "idle",
    model: "gpt-6-astra",
    created: Date.now() / 1000,
    inFlight: false,
    canSend: true,
    cwd,
  };
}
export function fixtureRuntime<T extends Partial<TestRuntimeView>>(value: T) {
  return {
    agents: [],
    rooms: [],
    tasks: [],
    monitors: [],
    complaints: [],
    projects: [],
    events: [],
    peerTeams: [],
    requests: [],
    rules: [],
    work: [],
    nativeNotices: [],
    sidebarOrder: null,
    rateLimits: null,
    rateLimitsByAccount: {},
    ...value,
  } satisfies TestRuntimeView;
}
export function fixtureSnapshot(
  value: Omit<EntityFixtureState, "runtime" | "edges"> & {
    runtime: Partial<TestRuntimeView>;
    threads?: Agent[];
    edges?: EntityFixtureState["edges"];
  },
): EntityFixtureState {
  return {
    ...value,
    edges: value.edges ?? [],
    runtime: fixtureRuntime(value.runtime),
  };
}
export type SyncFixture = Parameters<typeof handleEntitySyncFixtureRequest>[2];
export type NotifyResources = Parameters<
  NonNullable<SyncFixture["onStreamReady"]>
>[0];
type UnionKeys<T> = T extends unknown ? keyof T : never;
type WithAbsentFields<T, All = T> = T extends unknown
  ? T & Partial<Record<Exclude<UnionKeys<All>, keyof T>, never>>
  : never;
export type FixtureRequest = {
  [Path in ApiPostPath]: { path: Path; body: WithAbsentFields<PostBody<Path>> };
}[ApiPostPath];
// The renderer under test writes these requests using the generated API contract.
export function fixtureRequest<Path extends ApiPostPath>(
  path: Path,
  body: unknown,
): Extract<FixtureRequest, { path: Path }>;
export function fixtureRequest(path: string, body: unknown): FixtureRequest;
export function fixtureRequest(path: string, body: unknown): FixtureRequest {
  return { path, body } as FixtureRequest;
}
export const fixtureContentTypes: Record<string, string> = {
  ".js": "text/javascript",
  ".css": "text/css",
  ".html": "text/html",
};

// Fixture HTTP responses are produced by these specs using the generated route contracts.
type FixtureResponse = Pick<import("@playwright/test").APIResponse, "json">;
type FixtureEntity = JsonInputOf<import("../playwright.ts").SyncEntity>;
// The fixture sync route writes modern entity documents, not legacy payload envelopes.
export function readFixtureResponse(
  _path: "/api/sync/pull",
  response: FixtureResponse,
): Promise<
  Omit<
    Extract<
      import("../../src/apiTypes.ts").GetResult<"/api/sync/pull">,
      { documents: unknown }
    >,
    "documents"
  > & { documents: FixtureEntity[] }
>;
export function readFixtureResponse<
  Path extends import("../../src/apiTypes.ts").ApiGetPath,
>(
  _path: Path,
  response: FixtureResponse,
): Promise<import("../../src/apiTypes.ts").GetResult<Path>>;
export async function readFixtureResponse(
  _path: string,
  response: FixtureResponse,
): Promise<unknown> {
  return response.json();
}

export function fixtureAgentEntity(
  documents: Exclude<
    ReturnType<typeof import("../playwright.ts").entityPullFixture>,
    { reset: true }
  >["documents"],
  id: string,
) {
  return documents
    .filter(
      (
        document: (typeof documents)[number] & { op?: unknown },
      ): document is Extract<FixtureEntity, { op: "upsert" }> =>
        document.op === "upsert",
    )
    .find(
      (
        entity,
      ): entity is Extract<
        FixtureEntity,
        { op: "upsert"; collection: "agent" }
      > => entity.collection === "agent" && entity.id === id,
    );
}
