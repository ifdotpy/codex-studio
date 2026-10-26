import type { components } from "../../src/generated/api.ts";
import { build } from "esbuild";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { fixtureOrigin } from "./performance-fixture.ts";
import type {} from "./sidebar-incremental-render.fixture.tsx";
import assert from "node:assert/strict";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, readApiSchemaHash } from "../playwright.ts";

test("Sidebar incremental rows and current callbacks @performance", async ({
  page,
}) => {
  test.setTimeout(60_000);
  const root = join(import.meta.dirname, "../../");
  const directory = await mkdtemp(
    join(tmpdir(), "studio-sidebar-incremental-"),
  );
  const workspaceId = "c".repeat(32);
  const bundle = join(directory, "fixture.js");
  let server;
  try {
    await build({
      entryPoints: [
        new URL("./sidebar-incremental-render.fixture.tsx", import.meta.url)
          .pathname,
      ],
      absWorkingDir: root,
      bundle: true,
      jsx: "automatic",
      format: "esm",
      platform: "browser",
      outfile: bundle,
      define: { "process.env.NODE_ENV": '"production"' },
      plugins: [
        {
          name: "isolated-sidebar-probes",
          setup(build) {
            build.onResolve({ filter: /^\/src\// }, (args) => ({
              path: join(root, args.path),
            }));
            // The fixture measures the actual local projection and hook. The
            // resource transport has separate protocol and recovery contracts.
            build.onLoad({ filter: /resourceEvents\.ts$/ }, async () => ({
              contents: await readFile(
                new URL(
                  "./sidebar-resource-events.fixture.ts",
                  import.meta.url,
                ),
                "utf8",
              ),
              loader: "ts",
            }));
            build.onLoad(
              {
                filter:
                  /\/src\/(components\/Sidebar(?:Row)?\.tsx|sync\/client\.ts)$/,
              },
              async (args) => {
                let source = await readFile(args.path, "utf8");
                // Both implementations use the same probe. The old Sidebar
                // builds every row; the memoized view builds changed rows only.
                source = source.replace(
                  "const renderRow = (row: Agent) => {",
                  "const renderRow = (row: Agent) => {window.sidebarIncrementalFixtureRows++;",
                );
                source = source.replace(
                  "  const a = row;\n  return (",
                  "  window.sidebarIncrementalFixtureRows++; const a = row;\n  return (",
                );
                source = source.replaceAll(
                  "document.toJSON()",
                  "(window.sidebarIncrementalFixtureJsonReads++, document.toJSON())",
                );
                return {
                  contents: source,
                  loader: args.path.endsWith(".tsx") ? "tsx" : "ts",
                };
              },
            );
          },
        },
      ],
    });
    server = await createServer({
      configFile: false,
      root: directory,
      cacheDir: join(directory, "cache"),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "isolated-fixture-api",
          configureServer(vite) {
            vite.middlewares.use((request, response, next) => {
              const path = new URL(request.url ?? "", "http://fixture")
                .pathname;
              response.setHeader("X-Studio-API-Schema", readApiSchemaHash());
              if (path === "/") {
                response.setHeader("Content-Type", "text/html");
                response.end(
                  '<div id="root"></div><link rel="stylesheet" href="/fixture.css"><script type="module" src="/fixture.js"></script>',
                );
                return;
              }
              let value;
              if (path === "/api/sync/identity")
                value = { workspaceId } satisfies Pick<
                  components["schemas"]["SyncIdentityResponse"],
                  "workspaceId"
                >;
              if (path === "/api/session")
                value = {
                  token: "fixture",
                } satisfies components["schemas"]["SessionResponse"];
              if (path === "/api/sync/pull")
                value = {
                  workspaceId,
                  documents: [],
                  checkpoint: { seq: 22 },
                  maxSeq: 22,
                  initialHigh: 22,
                } satisfies Pick<
                  components["schemas"]["SyncPullResponse"],
                  | "workspaceId"
                  | "documents"
                  | "checkpoint"
                  | "maxSeq"
                  | "initialHigh"
                >;
              if (value) {
                response.setHeader("Content-Type", "application/json");
                response.end(JSON.stringify(value));
                return;
              }
              if (path.startsWith("/api/")) {
                response.statusCode = 500;
                response.end('{"error":"Isolated fixture"}');
                return;
              }
              next();
            });
          },
        },
      ],
    });
    await server.listen();
    const origin = fixtureOrigin(server);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/*", (route) => {
      if (
        !route.request().url().startsWith(`${origin}/`) ||
        !["GET", "HEAD"].includes(route.request().method())
      )
        return route.abort();
      return route.continue();
    });
    await page.goto(origin);
    await page.waitForFunction(
      () =>
        window.sidebarIncrementalFixtureReady &&
        window.sidebarIncrementalFixtureData,
    );
    await page.locator('[data-chat="agent-19"]').waitFor();
    await page.waitForTimeout(100);
    const before = await page.evaluate(() => ({
      rows: window.sidebarIncrementalFixtureRows,
      json: window.sidebarIncrementalFixtureJsonReads,
      renders: window.sidebarIncrementalFixtureRenders,
    }));
    for (let checks = 1; checks <= 3; checks++) {
      await page.evaluate(async (checks) => {
        await window.sidebarIncrementalWrite(
          "rule",
          "r",
          { id: "r", checks },
          100 + checks,
        );
      }, checks);
      await page.waitForFunction((checks) => {
        const rule = window.sidebarIncrementalFixtureData?.runtime.rules[0];
        return rule && "checks" in rule && rule.checks === checks;
      }, checks);
      await page.waitForTimeout(30);
    }
    const after = await page.evaluate(() => ({
      rows: window.sidebarIncrementalFixtureRows,
      json: window.sidebarIncrementalFixtureJsonReads,
      renders: window.sidebarIncrementalFixtureRenders,
    }));
    assert.equal(
      after.rows - before.rows,
      0,
      "rule changes retain all row views",
    );
    assert.equal(
      after.json - before.json,
      0,
      "retained immutable RxDB snapshots require no document conversion",
    );
    assert.equal(
      after.renders - before.renders,
      3,
      "empty creation reconciliation does not add a render",
    );
    assert.deepEqual(
      await page.evaluate(() =>
        window.sidebarIncrementalDocumentVersionProof(),
      ),
      {
        sameVersion: true,
        newVersion: true,
        stableNewVersion: true,
        immutableOldVersion: true,
      },
      "RxDB caches immutable document instances per revision",
    );
    await page.locator('[data-sidebar-id="key-row"]').press("ArrowDown");
    assert.deepEqual(
      await page.evaluate(() => window.sidebarIncrementalFixtureActions.at(-1)),
      ["key", 3],
    );
    await page.locator('[data-sidebar-id="key-row"]').dispatchEvent("dragover");
    assert.deepEqual(
      await page.evaluate(() => window.sidebarIncrementalFixtureActions.at(-1)),
      ["drag", 3],
    );
    await page.locator('[data-chat="agent-19"]').click();
    assert.deepEqual(
      await page.evaluate(() => window.sidebarIncrementalFixtureActions.at(-1)),
      ["agent-19", 3],
      "an unchanged row calls the current committed callback",
    );
    const unchanged = await page.evaluate(
      () => window.sidebarIncrementalFixtureRenders,
    );
    const conversionsBeforeReady = await page.evaluate(
      () => window.sidebarIncrementalFixtureJsonReads,
    );
    await page.evaluate(() => window.sidebarIncrementalReadyAgain());
    await page.waitForFunction(
      (prior) => window.sidebarIncrementalFixtureRenders > prior,
      unchanged,
    );
    assert.equal(
      await page.evaluate(() => window.sidebarIncrementalFixtureRenders),
      unchanged + 1,
      "a changed ready marker republishes the same projection once",
    );
    assert.equal(
      await page.evaluate(() => window.sidebarIncrementalFixtureJsonReads),
      conversionsBeforeReady,
      "republishing the same projection reuses cached RxDB documents",
    );
    const rowsBeforeAgent = await page.evaluate(
      () => window.sidebarIncrementalFixtureRows,
    );
    await page.evaluate(async () => {
      const value = {
        ...window.sidebarIncrementalFixtureAgent,
        name: "Current chat",
        status: "failed",
      };
      await window.sidebarIncrementalWrite("agent", value.id, value, 200);
    });
    await page
      .locator('[data-chat="agent-19"]')
      .getByText("Current chat", { exact: true })
      .waitFor();
    assert.equal(
      await page.evaluate(() => window.sidebarIncrementalFixtureRows),
      rowsBeforeAgent + 1,
      "the changed agent immediately replaces exactly its row view",
    );
    await page.evaluate(() =>
      window.sidebarIncrementalWrite("agent", "agent-19", {}, 201, true),
    );
    await page.locator('[data-chat="agent-19"]').waitFor({ state: "detached" });
    await page.evaluate(() =>
      window.sidebarIncrementalWrite(
        "agent",
        "agent-19",
        window.sidebarIncrementalFixtureAgent,
        200,
      ),
    );
    await page.waitForTimeout(50);
    assert.equal(
      await page.locator('[data-chat="agent-19"]').count(),
      0,
      "a stale sequence cannot restore a deleted agent",
    );
    await page.evaluate(() =>
      window.sidebarIncrementalWrite(
        "agent",
        "agent-19",
        { ...window.sidebarIncrementalFixtureAgent, name: "Restored" },
        202,
      ),
    );
    await page
      .locator('[data-chat="agent-19"]')
      .getByText("Restored", { exact: true })
      .waitFor();
    assert.deepEqual(errors, []);
  } finally {
    await server?.close();
    await rm(directory, { recursive: true, force: true });
  }
});
