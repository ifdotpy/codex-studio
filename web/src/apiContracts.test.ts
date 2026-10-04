import { expect, it } from "vitest";
import { serializePrimitiveParam } from "openapi-fetch";
import type { ApiGetContract, ApiPostContract } from "./apiContracts";

type FixturePaths = {
  "/items": {
    get: {
      parameters: {
        query: { cursor: string; limit?: number };
      };
      responses: {
        "200": { content: { "application/json": { items: string[] } } };
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
        "200": { content: { "application/json": { id: string } } };
      };
    };
  };
  "/health": {
    get: {
      responses: {
        "200": { content: { "application/json": { healthy: boolean } } };
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
  // @ts-expect-error enum values are closed to the declared wire strings
  fixturePost("/items", { id: "1", state: "pending" });
  // @ts-expect-error ETag caching must retain the 304 metadata destination
  fixtureGet("/health", { etag: "tag" });
}
void compileTimeContractAssertions;

it("serializes a typed query scalar using OpenAPI's default encoding", () => {
  expect(serializePrimitiveParam("cursor", "next page")).toBe(
    "cursor=next%20page",
  );
});
