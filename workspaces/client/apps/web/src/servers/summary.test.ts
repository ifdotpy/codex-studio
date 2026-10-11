import { expect, it } from "vitest";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import {
  fetchServerSummary,
  isSummaryRequest,
  SchemaMismatchError,
  summaryFailure,
} from "./summary";
const server = {
  id: "remote",
  label: "Remote",
  origin: "https://remote.tailnet.ts.net",
};
it("permits only the exact read-only summary path for the owning server", () => {
  expect(
    isSummaryRequest(new Request(server.origin + "/api/ui-summary"), server),
  ).toBe(true);
  for (const path of [
    "/api/session",
    "/api/sync/pull",
    "/api/ui-summary?server=B",
    "/api/ui-summary#B",
    "/api/ui-summary/",
  ])
    expect(isSummaryRequest(new Request(server.origin + path), server)).toBe(
      false,
    );
  expect(
    isSummaryRequest(
      new Request(server.origin + "/api/ui-summary", {
        method: "POST",
        body: "{}",
      }),
      server,
    ),
  ).toBe(false);
  expect(
    isSummaryRequest(
      new Request("https://other.tailnet.ts.net/api/ui-summary"),
      server,
    ),
  ).toBe(false);
});

it("tells another Studio version from a server that does not answer", async () => {
  const answer = (hash: string) => {
    (globalThis as { window?: unknown }).window = {
      codexDesktop: {
        serverCredentialAction: async () => ({
          status: 200,
          headers: [["x-studio-api-schema", hash]],
          body: "{}",
        }),
      },
    };
  };
  try {
    answer("another-hash");
    const other = await fetchServerSummary(
      server,
      new AbortController().signal,
    ).catch((error: unknown) => error);
    expect(other).toBeInstanceOf(SchemaMismatchError);
    expect(summaryFailure(other)).toBe("schema-mismatch");
    answer(API_SCHEMA_HASH);
    const same = await fetchServerSummary(
      server,
      new AbortController().signal,
    ).catch((error: unknown) => error);
    expect(same).not.toBeInstanceOf(SchemaMismatchError);
    expect(summaryFailure(new Error("timeout"))).toBe("offline");
  } finally {
    delete (globalThis as { window?: unknown }).window;
  }
});
