import { expect, test } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

test("server view reads preference state without a second sync client", async ({
  page,
}) => {
  const server = await fixture("Local");
  const requests = { pulls: [], streams: [], sessions: [] };
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (
      url.pathname === "/api/sync/pull" &&
      url.searchParams.get("scope") === "state:entities:v1"
    )
      requests.pulls.push(url);
    if (url.pathname === "/api/sync/stream") requests.streams.push(url);
    if (url.pathname === "/api/session") requests.sessions.push(url.href);
  });

  await page.goto(`${server.origin}/?studio-server=local`);
  await expect(page.getByRole("main")).toBeVisible();
  await expect
    .poll(() => ({
      mainStreams: requests.streams.filter((url) => {
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        return resources.some((resource) => resource.kind === "drafts");
      }).length,
      sessions: requests.sessions.length,
    }))
    .toEqual({ mainStreams: 2, sessions: 2 });
  expect(
    requests.pulls.some(
      (url) => url.searchParams.has("reset") || url.searchParams.has("fresh"),
    ),
  ).toBe(true);
  expect(
    requests.pulls.filter(
      (url) => !url.searchParams.has("reset") && !url.searchParams.has("fresh"),
    ),
  ).toHaveLength(0);
  expect(
    requests.streams.filter((url) => {
      const resources = JSON.parse(url.searchParams.get("resources") || "[]");
      return resources.length === 1 && resources[0].kind === "state";
    }),
  ).toHaveLength(0);
});
