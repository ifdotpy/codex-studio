import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import { required } from "../teams/helpers.ts";
import { createServer } from "vite";
import type {} from "./ux-request-recovery-browser.fixture.tsx";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  accountTileButton,
  entityPullFixtureForRequest,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
} from "../playwright.ts";
// Real UI components with isolated HTTP responses. No model or user state.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Ux request recovery browser", async ({
  browser: testBrowser,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const cache = await mkdtemp(join(tmpdir(), "studio-ux-request-recovery-"));

  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(root, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "request-recovery-fixture",
        configureServer(server) {
          server.middlewares.use((req, res, next) => {
            if (new URL(req.url || "/", "http://fixture").pathname !== "/")
              return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/errors/ux-request-recovery-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  const results = [];
  const selectedCases: string[] = [];
  const syncIdentity = syncIdentityFixture();
  const syncSnapshot = {
    ...fixtureSnapshot([]),
    runtime: {
      ...fixtureSnapshot([]).runtime,
      nativeNotices: [],
      sidebarOrder: { groups: null, revision: 0 },
      rateLimits: null,
      rateLimitsByAccount: {},
    },
  };
  try {
    await server.listen();
    for (const mode of selectedCases.length
      ? selectedCases
      : [
          "picker",
          "claude",
          "deadline",
          "accounts",
          "accounts-write",
          "name",
          "move",
          "project-account",
          "confirmed",
          "rejected",
          "project-deadline",
          "unready",
          "transfer",
        ]) {
      const page = await runnerContext.newPage();
      await page.setViewportSize({ width: 390, height: 844 });
      page.setDefaultTimeout(3000);
      const writes: Extract<
          PostBody<"/api/projects">,
          { folder_id?: unknown }
        >[] = [],
        errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      let reads = 0,
        releaseOld: (() => void) | undefined;
      if (mode.endsWith("deadline"))
        await page.addInitScript(() => {
          const original = window.setTimeout;
          window.setTimeout = Object.assign(
            (callback: TimerHandler, delay?: number, ...args: unknown[]) =>
              original(callback, delay === 15000 ? 100 : delay, ...args),
            original,
          );
        });
      await page.route("**/api/**", async (route) => {
        const request = route.request(),
          url = new URL(request.url()),
          path = url.pathname;
        if (path === "/api/sync/identity")
          return route.fulfill({
            json: syncIdentity,
            headers: apiSchemaHeaders(),
          });
        if (path === "/api/sync/protocol")
          return route.fulfill({
            json: syncProtocolFixture(),
            headers: apiSchemaHeaders(),
          });
        if (path === "/api/sync/stream") {
          const resources: import("../playwright.ts").ResourceRef[] =
            JSON.parse(url.searchParams.get("resources") || "[]");
          const baseline = {
            protocol: 4,
            workspaceId: syncIdentity.workspaceId,
            epoch: "fixture",
            revision: 1,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision: 1,
            })),
          };
          return route.fulfill({
            contentType: "text/event-stream",
            body: apiSchemaHandshakeSse(
              `event: resources\ndata: ${JSON.stringify(baseline)}\n\n`,
            ),
            headers: apiSchemaHeaders(),
          });
        }
        if (path === "/api/sync/pull")
          return route.fulfill({
            json: {
              workspaceId: syncIdentity.workspaceId,
              ...entityPullFixtureForRequest(syncSnapshot, request.url()),
            },
            headers: apiSchemaHeaders(),
          });
        if (path === "/api/models")
          return route.fulfill({
            json: { data: [] },
            headers: apiSchemaHeaders(),
          });
        if (path === "/api/directories") {
          const typed = url.searchParams.get("path") || "/project";
          return route.fulfill({
            json: {
              path: typed === "~/work" ? "/home/user/work" : typed,
              directories: [],
            },
            headers: apiSchemaHeaders(),
          });
        }
        if (path === "/api/claude/session") {
          reads++;
          if (mode === "deadline") return;
          if (reads === 1)
            return route.fulfill({
              status: 503,
              json: { error: "Settings unavailable" },
              headers: apiSchemaHeaders(),
            });
          return route.fulfill({
            json: {
              version: "fixture",
              settings: { permissionMode: "default", thinking: true },
              turns: [],
            },
            headers: apiSchemaHeaders(),
          });
        }
        if (path === "/api/accounts") {
          const sequence = ++reads;
          if (sequence === 1)
            await new Promise<void>((resolve) => {
              releaseOld = resolve;
            });
          return route.fulfill({
            json: {
              archivedAccounts: [],
              logins: [],
              supportsDisconnect: true,
              supportsDelete: true,
              accounts: [
                {
                  id: "ready",
                  label: sequence === 1 ? "Old account" : "New account",
                  home: "/home/fixture/ready",
                  source: "Codex CLI",
                  status: "ready",
                },
              ],
              defaultAccountKey: "ready",
            },
            headers: apiSchemaHeaders(),
          });
        }
        if (path === "/api/limits")
          return route.fulfill({
            json: {
              accountKey: url.searchParams.get("account_key") || "default",
              at: Date.now() / 1000,
              data: null,
              error: null,
            },
            headers: apiSchemaHeaders(),
          });
        if (request.method() === "GET")
          return route.fulfill({
            status: 500,
            json: { error: `Unhandled fixture GET ${path}` },
            headers: apiSchemaHeaders(),
          });
        writes.push(request.postDataJSON());
        if (mode === "project-deadline" && writes.length === 1) return;
        if (mode === "rejected" && writes.length === 1)
          return route.fulfill({
            status: 400,
            json: { error: "Name already exists" },
            headers: apiSchemaHeaders(),
          });
        if (mode !== "confirmed" && writes.length === 1)
          return route.fulfill({
            status: 503,
            json: { error: "Response lost after commit" },
            headers: apiSchemaHeaders(),
          });
        return route.fulfill({
          json: { projectFolder: "review" },
          headers: apiSchemaHeaders(),
        });
      });
      try {
        await page.goto(
          `${required(server.resolvedUrls?.local[0])}?case=${mode}`,
        );
        if (mode === "picker") {
          const use = page.getByRole("button", {
            name: "Add project",
            exact: true,
          });
          await page.waitForFunction(
            () =>
              !document.querySelector<HTMLButtonElement>(
                ".directory-footer button",
              )?.disabled,
          );
          await page.getByLabel("Folder path").fill("/other");
          assert.equal(
            await use.isEnabled(),
            false,
            "A typed path must not select the old directory",
          );
          await page
            .getByRole("button", { name: "Open path", exact: true })
            .click();
          await page.waitForFunction(
            () =>
              !document.querySelector<HTMLButtonElement>(
                ".directory-footer button",
              )?.disabled,
          );
          await use.click();
          assert.deepEqual(
            await page.evaluate(() => window.uxRequestRecoveryBrowserSelected),
            ["/other"],
          );
          await page.getByLabel("Folder path").fill("~/work");
          await page
            .getByRole("button", { name: "Open path", exact: true })
            .click();
          await page.waitForFunction(
            () =>
              document.querySelector(".directory-location")?.textContent ===
              "/home/user/work",
          );
          assert.equal(
            await page.getByLabel("Folder path").inputValue(),
            "/home/user/work",
          );
        } else if (mode === "claude") {
          await page
            .getByRole("alert")
            .filter({ hasText: "Settings unavailable" })
            .waitFor();
          assert.equal(
            await page
              .getByRole("region", { name: "Claude settings" })
              .getAttribute("aria-busy"),
            "false",
          );
          assert.equal(
            await page
              .getByRole("button", {
                name: "Retry Claude settings",
                exact: true,
              })
              .count(),
            1,
          );
          await page
            .getByRole("button", { name: "Retry Claude settings", exact: true })
            .click();
          await page.waitForFunction(
            () => !document.querySelector("select")?.disabled,
          );
          assert.equal(await page.getByRole("alert").count(), 0);
        } else if (mode === "deadline") {
          await page
            .getByRole("alert")
            .filter({ hasText: "The server did not respond in time" })
            .waitFor();
          assert.equal(
            await page
              .getByRole("button", {
                name: "Retry Claude settings",
                exact: true,
              })
              .count(),
            1,
          );
        } else if (mode.startsWith("accounts")) {
          await page
            .getByRole("button", { name: "Refresh accounts", exact: true })
            .waitFor();
          await page.waitForFunction(() => !!document.querySelector("output"));
          await page
            .getByRole("button", { name: "Refresh accounts", exact: true })
            .click();
          for (let attempt = 0; attempt < 100 && !releaseOld; attempt++)
            await new Promise((resolve) => setTimeout(resolve, 10));
          assert.equal(typeof releaseOld, "function");
          await page
            .getByRole("button", {
              name:
                mode === "accounts-write"
                  ? "Use saved accounts"
                  : "Refresh accounts",
              exact: true,
            })
            .click();
          await page.getByText("New account", { exact: true }).waitFor();
          const oldResponse = page.waitForResponse((response) =>
            response.url().endsWith("/api/accounts"),
          );
          required(releaseOld)();
          await oldResponse;
          await page.evaluate(
            () =>
              new Promise((resolve) =>
                requestAnimationFrame(() => requestAnimationFrame(resolve)),
              ),
          );
          assert.equal(
            await page.locator("output").textContent(),
            "New account",
            "A late response must not replace a newer account list",
          );
        } else if (mode === "transfer") {
          const text = await page.locator('[role="status"]').innerText();
          assert.equal(text.includes("0 waiting"), false);
          assert.match(text, /History transfer in progress: 1 remaining/);
        } else if (mode === "unready") {
          assert.equal(
            await page
              .getByRole("button", { name: "Save accounts", exact: true })
              .count(),
            0,
            "An unchanged unready default has no save action",
          );
          await accountTileButton(
            page,
            "Default account for new chats",
            "Ready account",
          ).click();
          assert.equal(
            await page
              .getByRole("button", { name: "Save accounts", exact: true })
              .isEnabled(),
            true,
          );
        } else if (mode === "project-account") {
          const expired = accountTileButton(
            page,
            "Default account for new chats",
            "Expired account",
          );
          assert.equal(
            await expired.isDisabled(),
            true,
            "An account that cannot start work cannot become the default",
          );
          assert.equal(
            await page
              .getByRole("group", {
                name: "Default account for new chats",
                exact: true,
              })
              .getAttribute("data-value"),
            "ready",
          );
        } else {
          const name = [
            "name",
            "confirmed",
            "rejected",
            "project-deadline",
          ].includes(mode);
          if (name) await page.getByLabel("Folder name").fill("Review");
          if (mode === "move")
            await page.getByLabel("Move to folder").selectOption("review");
          await page
            .getByRole("button", {
              name: name
                ? "Create folder"
                : mode === "move"
                  ? "Move chat"
                  : "Save accounts",
              exact: true,
            })
            .click();
          await page.getByRole("alert").waitFor();
          if (mode === "rejected") {
            assert.equal(
              await page.getByLabel("Folder name").isEnabled(),
              true,
            );
            await page.getByLabel("Folder name").fill("Different name");
            await page
              .getByRole("button", { name: "Create folder", exact: true })
              .click();
            await page.waitForFunction(
              () => window.uxRequestRecoveryBrowserSaves === 1,
            );
            assert.equal(writes.length, 2);
            assert.equal(
              required(writes[0]).folder_id,
              required(writes[1]).folder_id,
            );
            assert.equal(required(writes[1]).name, "Different name");
            assert.deepEqual(errors, []);
            results.push({ mode, passed: true });
            continue;
          }
          assert.equal(
            await page
              .getByLabel(
                name
                  ? "Folder name"
                  : mode === "move"
                    ? "Move to folder"
                    : "Default account for new chats",
              )
              .isEnabled(),
            false,
            "Unconfirmed requests must retain their exact data",
          );
          await page
            .getByRole("button", {
              name:
                mode === "confirmed"
                  ? "Refresh project"
                  : "Retry saved request",
              exact: true,
            })
            .click();
          await page.waitForFunction(
            () =>
              window.uxRequestRecoveryBrowserSaves ===
              (window.location.search.includes("confirmed") ? 2 : 1),
          );
          assert.equal(writes.length, mode === "confirmed" ? 1 : 2);
          if (writes.length === 2) assert.deepEqual(writes[0], writes[1]);
        }
        assert.deepEqual(errors, []);
        results.push({ mode, passed: true });
      } catch (error) {
        await page.screenshot({ path: join(cache, `${mode}-failure.png`) });
        results.push({
          mode,
          passed: false,
          error: String(error instanceof Error ? error.message : error).slice(
            0,
            400,
          ),
        });
      } finally {
        releaseOld?.();
        await page.close();
      }
    }
    console.log(JSON.stringify({ results, evidence: cache }));
    assert.equal(results.filter((result) => !result.passed).length, 0);
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    if (results.every((result) => result.passed))
      await rm(cache, { recursive: true, force: true });
  }
});
