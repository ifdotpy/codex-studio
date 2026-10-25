import { expect, it } from "vitest";
import { serializePrimitiveParam } from "openapi-fetch";
import { post } from "./api";
import type { LocalQueueItem, QueueItem } from "./components/MessageQueue";
import type { components } from "./generated/api";
import type { JsonValue, Message } from "./types";
import type { OperationRequestBodyContent } from "openapi-typescript-helpers";
import type { ApiGetContract, ApiSuccessBodyFor } from "./apiContracts";
import type { paths } from "./generated/api";

type ApiRequestBodyFor<Operation> = NonNullable<
  OperationRequestBodyContent<Operation>
>;

type Equal<Actual, Expected> =
  (<Value>() => Value extends Actual ? 1 : 2) extends <
    Value,
  >() => Value extends Expected ? 1 : 2
    ? (<Value>() => Value extends Expected ? 1 : 2) extends <
        Value,
      >() => Value extends Actual ? 1 : 2
      ? true
      : false
    : false;
type Assert<Condition extends true> = Condition;
type NotNever<Value> = [Value] extends [never] ? false : true;

const desktopDiskStatusHasOnlyRenderedFields: Assert<
  Equal<keyof components["schemas"]["DiskSpaceStatus"], "level" | "message">
> = true;
void desktopDiskStatusHasOnlyRenderedFields;

// OpenAPI owns one boolean selector; callers cannot send a string or a second mode.
const streamTokenInterestIsBoolean: Assert<
  Equal<
    NonNullable<
      paths["/api/sync/stream"]["get"]["parameters"]["query"]
    >["tokenRates"],
    boolean | undefined
  >
> = true;
void streamTokenInterestIsBoolean;

// The transcript producer has no string-valued unavailable reasons.
const transcriptUnavailableIsNull: Assert<
  Equal<
    components["schemas"]["TranscriptStreamDelta"]["unavailable"],
    null | undefined
  >
> = true;
void transcriptUnavailableIsNull;

const transcriptDeltaHasNoItemRevisions: Assert<
  Equal<
    Extract<
      keyof components["schemas"]["TranscriptStreamDelta"],
      "itemRevisions"
    >,
    never
  >
> = true;
void transcriptDeltaHasNoItemRevisions;

type NumericSuccess = ApiSuccessBodyFor<{
  responses: { 200: { content: { "application/json": { value: number } } } };
}>;
type StringSuccess = ApiSuccessBodyFor<{
  responses: {
    "200": { content: { "application/json": { value: string } } };
  };
}>;
type EmptySuccess = ApiSuccessBodyFor<{ responses: { 204: {} } }>;
type OptionalBody = ApiRequestBodyFor<{
  requestBody?: { content: { "application/json": Record<string, never> } };
}>;
type NoBody = ApiRequestBodyFor<{ responses: { 204: {} } }>;
type ResponseStatusAssertions = [
  Assert<Equal<NumericSuccess, { value: number }>>,
  Assert<NotNever<NumericSuccess>>,
  Assert<Equal<StringSuccess, { value: string }>>,
  Assert<NotNever<StringSuccess>>,
  Assert<Equal<EmptySuccess, undefined>>,
  Assert<NotNever<EmptySuccess>>,
  Assert<Equal<OptionalBody, Record<string, never>>>,
  Assert<Equal<NoBody, never>>,
];
const responseStatusAssertions: ResponseStatusAssertions = [
  true,
  true,
  true,
  true,
  true,
  true,
  true,
  true,
];
void responseStatusAssertions;

type FixturePaths = {
  "/items": {
    get: {
      parameters: {
        query: { cursor: string; limit?: number };
      };
      responses: {
        200: { content: { "application/json": { items: string[] } } };
      };
    };
    post: {
      requestBody: {
        required: true;
        content: {
          "application/json": { id: string; state: "open" | "closed" };
        };
      };
      responses: {
        200: { content: { "application/json": { id: string } } };
      };
    };
  };
  "/health": {
    get: {
      responses: {
        200: { content: { "application/json": { healthy: boolean } } };
      };
    };
  };
  "/mutation": {
    post: {
      requestBody: {
        required: true;
        content: { "application/json": { name: string } };
      };
      responses: {
        "204": {};
      };
    };
  };
  "/discover": {
    post: {
      requestBody?: {
        content: { "application/json": Record<string, never> };
      };
      responses: {
        200: { content: { "application/json": { found: boolean } } };
      };
    };
  };
  "/no-body": {
    post: { responses: { 204: {} } };
  };
};
type FixtureOptions = {
  timeoutMs?: number;
  workspaceId?: string;
  sessionToken?: string;
  signal?: AbortSignal;
};
type FixtureMetadata = { etag?: string; notModified?: boolean };
declare const fixtureGet: ApiGetContract<
  FixturePaths,
  FixtureOptions,
  FixtureMetadata
>;

function compileTimeContractAssertions() {
  fixtureGet("/items", { query: { cursor: "next" } });
  fixtureGet("/items", {
    query: { cursor: "next", limit: 20 },
    timeoutMs: 1000,
  });
  fixtureGet("/health");
  fixtureGet("/health", {
    etag: "tag",
    readMetadata: {},
  });
  const readResult: Promise<{ healthy: boolean }> = fixtureGet("/health");
  void readResult;

  // @ts-expect-error a required query object must be supplied
  fixtureGet("/items");
  // @ts-expect-error the required cursor query member cannot be omitted
  fixtureGet("/items", { query: { limit: 20 } });
  // @ts-expect-error an unknown path is not part of the generated contract
  fixtureGet("/missing");
  // @ts-expect-error ETag caching must retain the 304 metadata destination
  fixtureGet("/health", { etag: "tag" });
}
void compileTimeContractAssertions;

type GeneratedDiscoverBody = ApiRequestBodyFor<
  NonNullable<paths["/api/accounts/discover"]["post"]>
>;
type GeneratedDiscoverAssertion = Assert<
  Equal<GeneratedDiscoverBody, Record<string, never>>
>;
const generatedDiscoverAssertion: GeneratedDiscoverAssertion = true;
void generatedDiscoverAssertion;

type TranscriptTurnId =
  components["schemas"]["TranscriptItemResponse"]["turnId"];
type AnalyticsTurnError = components["schemas"]["AnalyticsTurnRecord"]["error"];
type MessageWireAssertions = [
  Assert<Equal<Message["turnId"], TranscriptTurnId>>,
  Assert<Equal<Message["nativeError"], AnalyticsTurnError>>,
];
const messageWireAssertions: MessageWireAssertions = [true, true];
void messageWireAssertions;

const nullableTranscriptTurnId: Message["turnId"] = null;
const nullableAnalyticsError: Message["nativeError"] = null;
const jsonAnalyticsError: Message["nativeError"] = {
  message: "provider error",
  details: ["context"],
};
const jsonValueAssertion: JsonValue = jsonAnalyticsError;
void nullableTranscriptTurnId;
void nullableAnalyticsError;
void jsonValueAssertion;

const localOutboxQueueRow: QueueItem = {
  id: "local-message",
  text: "unsent locally",
  requestedDelivery: "next_turn",
  localDelivery: true,
};
const serverQueueRow: QueueItem = {
  id: "server-message",
  text: "queued",
  status: "pending",
  kind: "user",
  created: 1,
  localDelivery: false,
};
const localDeliveryStatus: Message["deliveryStatus"] = "paused";
const localQueueRow: LocalQueueItem = localOutboxQueueRow;
// @ts-expect-error local queue projections do not fabricate server `status`
const localQueueStatus: string = localQueueRow.status;
// @ts-expect-error local queue projections do not fabricate server `created`
const localQueueCreated: number = localQueueRow.created;
void serverQueueRow;
void localDeliveryStatus;
void localQueueStatus;
void localQueueCreated;

function generatedFacadeRequestAssertions() {
  post("/api/agents", { prompt: "Read", permission_mode: "auto" });
  // @ts-expect-error permission modes are constrained by the shared contract
  post("/api/agents", { prompt: "Read", permission_mode: "unknown" });
  // @ts-expect-error GET-only generated paths are not accepted by the POST facade
  post("/api/accounts", {});
  post("/api/accounts/discover", {});
  // @ts-expect-error the facade requires an explicit request body argument
  post("/api/accounts/discover");
  // @ts-expect-error an absent request body is not accepted as a body value
  post("/api/accounts/discover", undefined);
  // @ts-expect-error the generated request body has no declared fields
  post("/api/accounts/discover", { unexpected: true });
  // @ts-expect-error the account default operation requires its request body
  post("/api/accounts/default");
  // @ts-expect-error queue actions are limited to the generated discriminator values
  post("/api/queue", { action: "unknown" });
}
void generatedFacadeRequestAssertions;

it("serializes a typed query scalar using OpenAPI's default encoding", () => {
  expect(serializePrimitiveParam("cursor", "next page")).toBe(
    "cursor=next%20page",
  );
});

// Every public account identifies its provider, including legacy Codex profiles.
type AccountProvider =
  paths["/api/accounts"]["get"]["responses"][200]["content"]["application/json"]["accounts"][number]["provider"];
const accountProvider: Assert<Equal<AccountProvider, "codex" | "claude">> =
  true;
void accountProvider;

// Draft wire properties come from the Rust owner, including required updated.
type DraftContract = import("./generated/rustOwned").DraftVersion;
const draftContract: DraftContract = {
  id: "phone:chat",
  session: "chat",
  device: "phone",
  text: "x",
  updated: 1,
};
// @ts-expect-error A new draft must carry its version timestamp.
const missingDraftTimestamp: DraftContract = {
  id: "phone:chat",
  session: "chat",
  device: "phone",
  text: "x",
};
const extraDraftField: DraftContract = {
  ...draftContract,
  // @ts-expect-error New draft payloads contain only fields from the Rust owner.
  metadata: { legacy: true },
};
void extraDraftField;
void draftContract;
void missingDraftTimestamp;

// Both Markdown transports consume the serialized Rust parser output.
import type {
  MarkdownBlock,
  MarkdownNode,
  MarkdownTree,
} from "./generated/rustOwned";
const markdownNode: MarkdownNode = {
  text: "Hello.",
  sentences: [{ text: "Hello.", index: 0 }],
};
const markdownTree: MarkdownTree = { nodes: [markdownNode], length: 6 };
const markdownBlocks: MarkdownBlock[] = [
  markdownTree,
  { kind: "mermaid", source: "graph TD; A-->B" },
];
// @ts-expect-error Rich Markdown blocks have a closed set of kinds.
const invalidMarkdownBlock: MarkdownBlock = { kind: "script", source: "run()" };
const markdownParserContract: Assert<
  Equal<
    Awaited<ReturnType<typeof import("./wasm/client").parseMarkdown>>,
    MarkdownBlock[]
  >
> = true;
void markdownBlocks;
void invalidMarkdownBlock;
void markdownParserContract;

// Desktop and local storage adapters consume their existing contract owners.
type DesktopDeclaration = import("../../desktop/bridge").CodexDesktop;
type WebDesktopDeclaration = import("./desktop").DesktopBridge;
type PreferenceDeclaration =
  import("./components/accountPreference").PreferenceAccount;
type PreferenceProjectDeclaration =
  import("./components/accountPreference").PreferenceProject;
type LocalSyncRow = import("./sync/client").SyncDocument;
type ProjectionSyncRow = import("./sync/entityProjection").EntityRow;
type LocalContractAssertions = [
  Assert<Equal<DesktopDeclaration, WebDesktopDeclaration>>,
  Assert<
    Equal<
      PreferenceDeclaration["provider"],
      Partial<components["schemas"]["Account"]>["provider"]
    >
  >,
  Assert<
    Equal<
      PreferenceDeclaration["status"],
      Partial<components["schemas"]["Account"]>["status"]
    >
  >,
  Assert<
    Equal<
      PreferenceProjectDeclaration,
      Pick<
        components["schemas"]["ProjectEntityDto"],
        "accountKey" | "accountKeys" | "accountGroups"
      >
    >
  >,
  Assert<
    Equal<
      Parameters<NonNullable<DesktopDeclaration["openSignIn"]>>[0],
      string | { url: string; forward: import("./desktop").SignInForward }
    >
  >,
  Assert<Equal<LocalSyncRow, ProjectionSyncRow>>,
  Assert<
    Equal<
      Required<Omit<LocalSyncRow, "entity">>,
      components["schemas"]["SyncDocument"]
    >
  >,
];
const localContractAssertions: LocalContractAssertions = [
  true,
  true,
  true,
  true,
  true,
  true,
  true,
];
void localContractAssertions;

// Entity room DTOs contain fixed radio identities; API responses retain tuples.
type TupleResponse = ApiSuccessBodyFor<{
  responses: {
    200: {
      content: {
        "application/json": {
          identity: [string | null, number | null, number];
        };
      };
    };
  };
}>;
const tupleResponsePreserved: Assert<
  Equal<TupleResponse["identity"], [string | null, number | null, number]>
> = true;
void tupleResponsePreserved;
