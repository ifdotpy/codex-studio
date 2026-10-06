import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import {
  legacySnapshotRoute,
  syncProtocolFixture,
  test,
} from "../playwright.mjs";

test("legacy first-load fallback is deleted with the renderer fallback", async ({
  page,
}) => {
  test.setTimeout(60000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const require = createRequire(join(repo, "web/package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const server = await createServer({
    configFile: false,
    root: join(repo, "web"),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  let snapshotReads = 0;
  const requests = [];
  page.on("request", (request) =>
    requests.push(
      `${request.method()} ${new URL(request.url()).pathname}${new URL(request.url()).search}`,
    ),
  );
  page.on("pageerror", (error) =>
    console.error("legacy fallback page error", error.message),
  );
  try {
    await page.route("**/check", (route) =>
      route.fulfill({ body: "<div id='root'></div>" }),
    );
    await page.route("**/api/session", (route) =>
      route.fulfill({ json: { token: "fixture-token" } }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        status: 503,
        json: { error: "Fixture sync unavailable" },
      }),
    );
    await page.route("**/api/sync/protocol", (route) =>
      route.fulfill({ json: syncProtocolFixture() }),
    );
    await page.route("**/api/sync/stream**", (route) =>
      route.fulfill({ status: 503, body: "fixture unavailable" }),
    );
    await page.route("**/api/sync/pull**", (route) =>
      route.fulfill({
        status: 503,
        json: { error: "Entity sync is unavailable" },
      }),
    );
    await page.route(legacySnapshotRoute, (route) => {
      snapshotReads++;
      return route.fulfill({
        json: {
          token: "legacy-token",
          stateDir: "legacy-fixture",
          threads: [],
          chats: [],
          edges: [],
          runtime: {
            agents: [],
            rooms: [],
            tasks: [],
            monitors: [],
            complaints: [],
            requests: [],
            projects: [],
            peerTeams: [],
            events: [],
            work: [],
            rules: [],
            nativeNotices: [],
            sidebarOrder: [],
          },
        },
      });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const initialFallback = page.waitForResponse((response) =>
      legacySnapshotRoute.test(response.url()),
    );
    await page.evaluate(async () => {
      const { useSnapshot } = await import("/src/hooks.ts");
      const ReactModule = await import("/node_modules/.vite/deps/react.js");
      const DomModule =
        await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = ReactModule.default || ReactModule;
      const { createRoot } = DomModule.default || DomModule;
      const node = document.getElementById("root");
      createRoot(node).render(
        React.createElement(function Harness() {
          window.snapshot = useSnapshot();
          return null;
        }),
      );
    });
    await initialFallback;
    assert.ok(
      snapshotReads >= 1,
      "initial subscription failure uses the temporary snapshot fallback",
    );
    const refreshFallback = page.waitForResponse((response) =>
      legacySnapshotRoute.test(response.url()),
    );
    await page.evaluate(() => window.snapshot.refresh(false));
    await refreshFallback;
    await page.waitForFunction(
      () => window.snapshot?.data?.stateDir === "legacy-fixture",
      undefined,
      { timeout: 10000 },
    );
    assert.ok(
      snapshotReads >= 2,
      "refresh failure uses the temporary snapshot fallback",
    );
  } finally {
    await server.close();
  }
});
