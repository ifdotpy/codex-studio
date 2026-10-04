import { expect, it } from "vitest";
import { serializePrimitiveParam } from "openapi-fetch";
import { post } from "./api";
import type {
  ApiSuccessBodyFor,
  ApiGetContract,
  ApiPostContract,
  ApiRequestBodyFor,
  ApiSyncGetContract,
} from "./apiContracts";
import type { paths } from "./generated/api";

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
declare const fixturePost: ApiPostContract<FixturePaths, FixtureOptions>;
declare const fixtureSyncGet: ApiSyncGetContract<
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
  fixturePost("/items", { id: "1", state: "open" });
  fixturePost("/mutation", { name: "sample" });
  fixturePost("/discover", {});
  fixtureSyncGet("/items", { query: { cursor: "same-query" } });
  const readResult: Promise<{ healthy: boolean }> = fixtureGet("/health");
  const writeResult: Promise<{ id: string }> = fixturePost("/items", {
    id: "1",
    state: "open",
  });
  const emptyWriteResult: Promise<undefined> = fixturePost("/mutation", {
    name: "empty response",
  });
  void readResult;
  void writeResult;
  void emptyWriteResult;

  // @ts-expect-error a required query object must be supplied
  fixtureGet("/items");
  // @ts-expect-error the required cursor query member cannot be omitted
  fixtureGet("/items", { query: { limit: 20 } });
  // @ts-expect-error a GET-only path cannot be called with the POST helper
  fixturePost("/health", {});
  // @ts-expect-error an unknown path is not part of the generated contract
  fixtureGet("/missing");
  // @ts-expect-error the body is required for this operation
  fixturePost("/items", undefined);
  // @ts-expect-error this operation does not accept a request body
  fixturePost("/no-body", {});
  // @ts-expect-error enum values are closed to the declared wire strings
  fixturePost("/items", { id: "1", state: "pending" });
  // @ts-expect-error ETag caching must retain the 304 metadata destination
  fixtureGet("/health", { etag: "tag" });
  // @ts-expect-error a fixed-deadline read cannot omit its required query
  fixtureSyncGet("/items");
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

function generatedFacadeRequestAssertions() {
  post("/api/accounts/discover", {});
  // @ts-expect-error the facade requires an explicit request body argument
  post("/api/accounts/discover");
}
void generatedFacadeRequestAssertions;

it("serializes a typed query scalar using OpenAPI's default encoding", () => {
  expect(serializePrimitiveParam("cursor", "next page")).toBe(
    "cursor=next%20page",
  );
});
