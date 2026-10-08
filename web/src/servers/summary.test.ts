import { expect, it } from "vitest";
import { isSummaryRequest } from "./summary";
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
