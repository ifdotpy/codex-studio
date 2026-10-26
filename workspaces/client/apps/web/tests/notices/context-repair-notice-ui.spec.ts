import type { Agent } from "../../src/types.ts";
import { fixtureServerOrigin } from "../mobile/browser-fixture.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type {} from "./context-repair-notice-ui.fixture.tsx";
import { test } from "../playwright.ts";
// Render the actual banner: ordinary monitor waits are quiet, failures stay visible.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("Context repair notice ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const cache = await mkdtemp(join(tmpdir(), "context-repair-notice-vite-"));
  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "context-repair-notice-fixture",
        configureServer(vite) {
          vite.middlewares.use((req, res, next) => {
            if (req.url !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/notices/context-repair-notice-ui.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    const errors: string[] = [],
      writes: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "GET")
        writes.push(route.request().url());
      await route.fulfill({ json: {} });
    });
    await page.goto(fixtureServerOrigin(server));
    await page.waitForFunction(
      () => typeof window.applyContextRepairSnapshot === "function",
    );
    const error =
      "Context repair waits for monitors: 3c403ca5-4143-5750-a061-dd19ab5aab5c";
    const agent: Agent = {
      id: "fixture",
      name: "Fixture",
      // @ts-expect-error Legacy source value is deliberately preserved by this presentation fixture.
      source: "native",
      status: "queued",
      model: "gpt-6-astra",
      created: 1,
      threadId: "native-fixture",
      accountKey: "default",
      epoch: 0,
      error,
      contextRepairWait: { scope: "local", error },
    };
    let cases = 0;
    const check = async (
      label: string,
      value: Agent,
      visible: boolean,
      text?: RegExp,
    ) => {
      await page.evaluate(
        (snapshot) => window.applyContextRepairSnapshot(snapshot),
        JSON.stringify({
          label,
          agent: value,
        }),
      );
      await page.locator(`main[data-case="${label}"]`).waitFor();
      assert.equal(
        await page.locator(".native-error").count(),
        visible ? 1 : 0,
        label,
      );
      if (text)
        assert.match(await page.locator(".native-error").innerText(), text);
      assert.deepEqual(
        JSON.parse(await page.locator("output").innerText()),
        JSON.parse(JSON.stringify(value)),
        "Presentation preserves saved state",
      );
      cases++;
    };
    await check("ordinary-monitor", agent, false);
    await check(
      "missing-proof",
      { ...agent, contextRepairWait: undefined },
      true,
    );
    await check(
      "native-scope",
      { ...agent, contextRepairWait: { scope: "native", error } },
      true,
    );
    await check(
      "unknown-scope",
      // @ts-expect-error Deliberately missing contextRepairWait.scope tests a legacy malformed notice.
      { ...agent, contextRepairWait: { error } },
      true,
    );
    await check(
      "mismatched-error",
      { ...agent, error: "disk I/O error" },
      true,
      /disk I\/O error/,
    );
    await check("failed-state", { ...agent, status: "failed" }, true);
    const receiptError =
      "Context repair waits for the existing native recovery receipt";
    await check(
      "unknown-receipt",
      {
        ...agent,
        error: receiptError,
        contextRepairWait: { scope: "local", error: receiptError },
      },
      true,
      /receipt/,
    );
    await check(
      "native-block",
      {
        ...agent,
        nativeThreadBlock: {
          threadId: agent.threadId ?? null,
          error: {
            codexErrorInfo: "badRequest",
            message: "Native request rejected",
          },
        },
      },
      true,
      /Native request rejected/,
    );
    await check(
      "capacity-retry",
      {
        ...agent,
        capacityRetry: {
          id: "retry",
          threadId: agent.threadId ?? null,
          epoch: 0,
          accountKey: "default",
          status: "scheduled",
          dueAt: Date.now() / 1000 + 60,
          updatedAt: Date.now() / 1000,
        },
      },
      true,
      /Automatic retry/,
    );
    await check(
      "real-failure",
      {
        ...agent,
        error: {
          codexErrorInfo: "responseStreamDisconnected",
          message: "stream disconnected before completion",
        },
      },
      true,
    );
    await check("wait-after-failure", agent, false);
    assert.deepEqual(errors, []);
    assert.deepEqual(writes, []);
    console.log(
      JSON.stringify({
        pass: true,
        cases,
        noWrites: true,
        realComponent: true,
      }),
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
