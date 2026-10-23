import { expect, test } from "../playwright.mjs";
import {
  fixture,
  fixtureWorkspaceId,
} from "../servers/multi-server-fixture.mjs";

for (const entry of ["/?studio-server=local", "/"]) {
  test(`server projection at ${entry} reads preferences without a second sync client`, async ({
    page,
  }) => {
    const server = await fixture("Local");
    const requests = { pulls: [], streams: [] };
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (
        url.pathname === "/api/sync/pull" &&
        url.searchParams.get("scope") === "state:entities:v1"
      )
        requests.pulls.push(url);
      if (url.pathname === "/api/sync/stream") requests.streams.push(url);
    });

    try {
      await page.goto(`${server.origin}${entry}`);
      await expect(page.getByRole("main")).toBeVisible();
      await expect
        .poll(() =>
          requests.streams.some((url) =>
            JSON.parse(url.searchParams.get("resources") || "[]").some(
              (resource) => resource.kind === "drafts",
            ),
          ),
        )
        .toBe(true);
      await expect
        .poll(() =>
          page.evaluate(() =>
            localStorage.getItem("studio-server-workspace-id:local"),
          ),
        )
        .toBe(fixtureWorkspaceId("Local"));
      expect(
        requests.pulls.some(
          (url) =>
            url.searchParams.has("reset") || url.searchParams.has("fresh"),
        ),
      ).toBe(true);
      expect(
        requests.pulls.filter(
          (url) =>
            !url.searchParams.has("reset") && !url.searchParams.has("fresh"),
        ),
      ).toHaveLength(0);
      expect(
        requests.streams.filter((url) => {
          const resources = JSON.parse(
            url.searchParams.get("resources") || "[]",
          );
          return resources.length === 1 && resources[0].kind === "state";
        }),
      ).toHaveLength(0);
    } finally {
      await server.close();
    }
  });
}

test("paired screen preference writes keep their server API and storage", async ({
  page,
  context,
}) => {
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  const posts = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === "/api/sync/preferences" && request.method() === "POST")
      posts.push({ origin: url.origin, body: request.postDataJSON() });
  });
  try {
    await context.addInitScript(
      ({ origin, destination }) => {
        const fetchRequest = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (url.origin !== origin) return fetchRequest(request);
          const bytes = await request.clone().arrayBuffer();
          return fetchRequest(destination + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
        localStorage.setItem(
          "codex-project-tree:/same/project",
          JSON.stringify({ collapsed: "local-folder" }),
        );
        localStorage.setItem(
          ":server:remote:codex-project-tree:/same/project",
          JSON.stringify({ collapsed: "remote-folder" }),
        );
      },
      { origin: remote.invitation.origin, destination: remote.origin },
    );
    await page.goto(local.origin);
    await expect(page.locator("#message")).toBeVisible();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Servers", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Studio settings" });
    await dialog
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await dialog.getByLabel("Server address").fill(remote.invitation.origin);
    await dialog
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await dialog
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    await expect(page.locator("#chat-list")).toContainText("Remote chat");
    const field = JSON.stringify([
      "codex-project-tree:/same/project",
      "collapsed",
    ]);
    for (const [origin, value] of [
      [local.origin, "local-folder"],
      [remote.origin, "remote-folder"],
    ]) {
      await expect
        .poll(() =>
          posts.some(
            (post) =>
              post.origin === origin &&
              post.body.scope === "server" &&
              post.body.fields[field]?.value === value,
          ),
        )
        .toBe(true);
      expect(
        posts.some(
          (post) =>
            post.origin !== origin &&
            post.body.scope === "server" &&
            post.body.fields[field]?.value === value,
        ),
      ).toBe(false);
    }
    await expect
      .poll(() =>
        page.evaluate(() =>
          localStorage.getItem("studio-server-workspace-id:remote"),
        ),
      )
      .toBe(fixtureWorkspaceId("Remote"));
  } finally {
    await Promise.all([local.close(), remote.close()]);
  }
});
