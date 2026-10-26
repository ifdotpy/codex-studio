import type {
  SyncPullResponse,
  SyncEntity,
  SessionResponse,
} from "../playwright.ts";
import type {} from "./session-poll.fixture.tsx";
import type { ServerResponse } from "node:http";
import { assert } from "./assertions.ts";
import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  legacySnapshotRoute,
  syncIdentityFixture,
  test,
  createFixtureViteServer,
} from "../playwright.ts";
// Replicated state uses the current credential endpoint and reports failures.
import { fileURLToPath } from "node:url";
// This legacy workspace fixture intentionally carries a marker outside the DTO.
type LegacyWorkspace = Omit<
  Extract<SyncEntity, { collection: "workspace"; op: "upsert" }>,
  "value"
> & { value: unknown };
type LegacyPull = Omit<SyncPullResponse, "generation" | "documents"> & {
  documents: LegacyWorkspace[];
};

test("Session Poll Browser", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  let streamResponse: ServerResponse | undefined;
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "session-poll-stream-fixture",
        configureServer(vite) {
          vite.middlewares.use((request, response, next) => {
            if (request.url?.split("?", 1)[0] !== "/api/sync/stream") {
              next();
              return;
            }
            streamResponse = response;
            response.writeHead(200, {
              ...apiSchemaHeaders(),
              "Content-Type": "text/event-stream",
              "Cache-Control": "no-cache",
            });
            response.write(`${apiSchemaHandshakeSse()}retry: 60000\n\n`);
          });
        },
      },
    ],
  });
  await server.listen();
  const address = server.httpServer?.address();
  if (!address || typeof address === "string")
    throw new Error("Missing fixture server address");
  try {
    const page = testPage;
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let pulls = 0,
      snapshotReads = 0,
      sessions = 0,
      status = 200,
      token = "first";
    const identity = syncIdentityFixture();
    const workspaceId = identity.workspaceId;
    await page.route(legacySnapshotRoute, (route) => {
      snapshotReads++;
      return route.continue();
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><title>Session refresh</title><script type="module" src="/tests/chat/session-poll.fixture.tsx"></script>',
      }),
    );
    await page.route("**/api/session", (route) => {
      sessions++;
      return route.fulfill({
        status,
        headers: apiSchemaHeaders(),
        json:
          status === 200
            ? ({ token } satisfies SessionResponse)
            : { error: "Session unavailable" },
      });
    });
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: identity, headers: apiSchemaHeaders() }),
    );
    await page.route("**/api/sync/pull?*", (route) => {
      pulls++;
      const after = Number(
        new URL(route.request().url()).searchParams.get("after"),
      );
      return route.fulfill({
        json: {
          workspaceId,
          documents: after
            ? []
            : [
                {
                  op: "upsert",
                  collection: "workspace",
                  id: "current",
                  value: { marker: "replicated", stateDir: "fixture" },
                  seq: 1,
                },
              ],
          checkpoint: { seq: 1 },
          maxSeq: 1,
          initialHigh: 1,
        } satisfies LegacyPull,
        headers: apiSchemaHeaders(),
      });
    });
    await page.goto(`http://127.0.0.1:${address.port}/check`);

    await page.waitForFunction(
      () =>
        Reflect.get(
          window.sessionPollFixture?.data?.runtime ?? {},
          "marker",
        ) === "replicated",
    );
    await page.evaluate(() => window.sessionPollFixture.refresh());
    assert.ok(sessions >= 2);
    assert.equal(
      snapshotReads,
      0,
      "Replicated startup and refresh never download the full state",
    );
    assert.ok(pulls > 0, "Startup and refresh use the entity pull");
    token = "rotated";
    await page.evaluate(() => window.sessionPollFixture.refresh());
    assert.equal(
      await page.evaluate(() => window.sessionPollFixture.data?.token),
      "rotated",
    );
    assert.equal(
      await page.evaluate(() =>
        Reflect.get(window.sessionPollFixture.data?.runtime ?? {}, "marker"),
      ),
      "replicated",
    );
    for (const failure of [503, 404]) {
      status = failure;
      await page.evaluate(() => window.sessionPollFixture.refresh());
      assert.equal(
        await page.evaluate(() => window.sessionPollFixture.error),
        "Session unavailable",
      );
      assert.equal(
        await page.evaluate(() => window.sessionPollFixture.data?.token),
        "rotated",
      );
      assert.equal(
        snapshotReads,
        0,
        "A missing endpoint or server failure never invokes the old snapshot fallback",
      );
    }
    status = 200;
    token = "recovered";
    await page.evaluate(() => window.sessionPollFixture.refresh());
    assert.equal(
      await page.evaluate(() => window.sessionPollFixture.data?.token),
      "recovered",
    );
    assert.equal(
      await page.evaluate(() => window.sessionPollFixture.error),
      "",
    );
    assert.equal(snapshotReads, 0, "Never download the full state");
    assert.equal(snapshotReads, 0, "Never invoke the old snapshot fallback");
    assert.deepEqual(errors, []);
    console.log(
      "session polling PASS: entity pull, rotated token, visible error, missing endpoint rejection",
    );
  } finally {
    streamResponse?.end();
    await server.close();
  }
});
