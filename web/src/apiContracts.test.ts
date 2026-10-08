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
  requestedDelivery: "after_turn",
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
