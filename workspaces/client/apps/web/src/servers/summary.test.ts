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
  const answer = (hash: string, status = 200) => {
    (globalThis as { window?: unknown }).window = {
      codexDesktop: {
        serverCredentialAction: async () => ({
          status,
          headers: [["x-studio-api-schema", hash]],
          body: JSON.stringify({
            chats: [],
            projects: [],
            alerts: [],
            accounts: [],
            ready: true,
            busy: false,
            system: "fixture",
            agentsRunning: 0,
          }),
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
    await expect(
      fetchServerSummary(server, new AbortController().signal),
    ).resolves.toMatchObject({ ready: true, system: "fixture", accounts: [] });
    answer(API_SCHEMA_HASH, 503);
    const unavailable = await fetchServerSummary(
      server,
      new AbortController().signal,
    ).catch((error: unknown) => error);
    expect(unavailable).toBeInstanceOf(Error);
    expect(summaryFailure(unavailable)).toBe("offline");
  } finally {
    delete (globalThis as { window?: unknown }).window;
  }
});
