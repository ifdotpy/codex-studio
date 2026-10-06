import { apiSchemaHandshakeSse, test, expect } from "../playwright.mjs";
// Replicated state uses the current credential endpoint and reports failures.
import { fileURLToPath } from "node:url";
test("Session Poll Browser", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = testPage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let pulls = 0,
      sessions = 0,
      status = 200,
      token = "first";
    const workspaceId = "a".repeat(32);
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Session refresh</title>",
      }),
    );
    await page.route("**/api/session", (route) => {
      sessions++;
      return route.fulfill({
        status,
        json: status === 200 ? { token } : { error: "Session unavailable" },
      });
    });
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await page.route("**/api/sync/stream**", (route) =>
      route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(),
      }),
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
                  id: "entity:workspace:current",
                  seq: 1,
                  _deleted: false,
                  payload: JSON.stringify({
                    collection: "workspace",
                    id: "current",
                    value: { marker: "replicated", stateDir: "fixture" },
                  }),
                },
              ],
          checkpoint: { seq: 1 },
          maxSeq: 1,
          initialHigh: 1,
        },
      });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      const { useSnapshot } = await import("/src/hooks.ts");
      const r = await import("/node_modules/.vite/deps/react.js"),
        d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r,
        { createRoot } = d.default || d;
      const node = document.createElement("div");
      document.body.appendChild(node);
      createRoot(node).render(
        React.createElement(function Harness() {
          window.snapshot = useSnapshot();
          return null;
        }),
      );
    });
    await page.waitForFunction(
      () => window.snapshot?.data?.runtime?.marker === "replicated",
    );
    await page.evaluate(() => window.snapshot.refresh());
    assert.ok(sessions >= 2);
    assert.ok(pulls > 0, "Startup and refresh use the entity pull");
    token = "rotated";
    await page.evaluate(() => window.snapshot.refresh());
    assert.equal(
      await page.evaluate(() => window.snapshot.data.token),
      "rotated",
    );
    assert.equal(
      await page.evaluate(() => window.snapshot.data.runtime.marker),
      "replicated",
    );
    for (const failure of [503, 404]) {
      status = failure;
      await page.evaluate(() => window.snapshot.refresh());
      assert.equal(
        await page.evaluate(() => window.snapshot.error),
        "Session unavailable",
      );
      assert.equal(
        await page.evaluate(() => window.snapshot.data.token),
        "rotated",
      );
    }
    status = 200;
    token = "recovered";
    await page.evaluate(() => window.snapshot.refresh());
    assert.equal(
      await page.evaluate(() => window.snapshot.data.token),
      "recovered",
    );
    assert.equal(await page.evaluate(() => window.snapshot.error), "");
    assert.deepEqual(errors, []);
    console.log(
      "session polling PASS: entity pull, rotated token, visible error, missing endpoint rejection",
    );
  } finally {
    await server.close();
  }
});
