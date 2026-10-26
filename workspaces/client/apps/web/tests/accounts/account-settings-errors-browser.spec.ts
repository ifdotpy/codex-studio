import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { chromium, webkit } from "playwright-core";
import { createFixtureViteServer } from "../playwright.ts";
import type { Page, Locator } from "@playwright/test";
import type { JsonInput } from "../../src/apiTypes.ts";
import { accountErrorFailure } from "./account-errors-data.ts";
import {
  fixtureUrl,
  fixtureSnapshot,
  required,
  type SyncFixture,
  type NotifyResources,
} from "./fixture-helpers.ts";
import type {} from "./account-errors.fixture.tsx";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  test,
  browserExecutablePath,
  handleEntitySyncFixtureRequest,
} from "../playwright.ts";

test("Account settings errors", async () => {
  test.setTimeout(45_000);
  const root = fileURLToPath(new URL("../..//", import.meta.url));
  const negative = process.env.NEGATIVE === "1";
  const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
  const failure = accountErrorFailure;
  const workspaceId = "0123456789abcdef0123456789abcdef";
  const resourceStreams = new Set<{
    notify: NotifyResources;
    resources: Parameters<NotifyResources>[0];
  }>();
  const syncFixture: SyncFixture = {
    snapshot: fixtureSnapshot({
      runtime: {},
      stateDir: "fixture",
      chats: [],
      edges: [],
    }),
    workspaceId,
    epoch: "account-errors-fixture",
    onStreamReady(notify, resources) {
      const stream = { resources, notify };
      resourceStreams.add(stream);
      return () => resourceStreams.delete(stream);
    },
  };
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-account-errors-"));
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "account-error-fixture",
        configureServer(server) {
          server.middlewares.use("/fixture-data", (_req, res) => {
            res.setHeader("Content-Type", "application/json");
            res.end(JSON.stringify({ negative }));
          });
          server.middlewares.use((req, res, next) => {
            if (!handleEntitySyncFixtureRequest(req, res, syncFixture)) next();
          });
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<html><body><div id="root"></div><script type="module" src="/tests/accounts/account-errors.fixture.tsx"></script></body></html>',
            );
          });
        },
        load(id) {
          if (negative && id === join(root, "src/components/AccountSignIn.tsx"))
            return execFileSync(
              "git",
              ["show", "f72c2ce:web/src/components/AccountSignIn.tsx"],
              { cwd: root, encoding: "utf8" },
            );
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = await browserType.launch({
      headless: true,
      ...(browserType === chromium
        ? {
            executablePath: browserExecutablePath,
          }
        : {}),
    });
    const page = await browser.newPage({
      viewport: { width: 1100, height: 850 },
    });
    page.setDefaultTimeout(5000);
    const errors: string[] = [],
      mutations: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let limitsHttpFailure = false;
    let limitsRecovered = false;
    let limitsReads = 0;
    const limitsFailureBody = {
      error: failure.message,
      location: ["query", "account_key"],
    };
    await page.route("**/api/**", (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (pathname.startsWith("/api/sync/")) return route.continue();
      if (pathname === "/api/limits") limitsReads++;
      if (route.request().method() !== "GET") mutations.push(pathname);
      if (pathname === "/api/limits" && limitsHttpFailure)
        return route.fulfill({ status: 503, json: limitsFailureBody });
      if (pathname === "/api/limits" && limitsRecovered)
        return route.fulfill({
          json: {
            accountKey: "default",
            at: Date.now() / 1000,
            data: {
              rateLimits: {
                limitId: "codex",
                planType: "pro",
                primary: { usedPercent: 20, windowDurationMins: 300 },
                secondary: {
                  usedPercent: 20,
                  windowDurationMins: 10080,
                },
              },
            },
          },
        });
      return route.fulfill({
        json:
          pathname === "/api/accounts"
            ? {
                accounts: [],
                archivedAccounts: [],
                defaultAccountKey: "default",
                logins: [],
              }
            : pathname === "/api/rules"
              ? {
                  rules: [
                    {
                      id: "rule",
                      agent: "lead",
                      name: "Rule diagnostic",
                      kind: "low_workers",
                      error: failure,
                      text: "Saved rule text",
                    },
                  ],
                }
              : pathname === "/api/capabilities"
                ? { errors: [failure], managed: [], native: [] }
                : pathname === "/api/changes"
                  ? { scope: "chat", git: false, error: failure, files: [] }
                  : pathname === "/api/limits"
                    ? {
                        accountKey: "default",
                        error: failure.message,
                        data: null,
                      }
                    : {},
      });
    });
    await page.goto(`${fixtureUrl(server)}/check`);
    if (negative) {
      for (let i = 0; i < 100 && !errors.length; i++)
        await page.waitForTimeout(25);
      assert.ok(
        errors.some((error) =>
          /Objects are not valid as a React child|Minified React error #31/.test(
            error,
          ),
        ),
        "Original sign-in receipt renderer crashes on the structured payload",
      );
      console.log(
        "PASS negative control: original AccountSignIn throws React object-child error",
      );
    } else {
      async function details(
        expected: JsonInput,
        scope: Page | Locator = page,
      ) {
        const button = scope
          .getByRole("button", { name: "Error details", exact: true })
          .first();
        await button.click();
        assert.deepEqual(
          JSON.parse(
            required(
              await scope
                .getByRole("button", {
                  name: "Hide error details",
                  exact: true,
                })
                .first()
                .locator("..")
                .locator(":scope > span")
                .last()
                .textContent(),
            ),
          ),
          expected,
        );
        assert.equal(await page.locator("[data-shell]").count(), 1);
      }
      await page
        .getByText(failure.message, { exact: true })
        .waitFor({ state: "attached" });
      const changed = {
        message: "Updated provider error",
        code: "new-code",
        metadata: { region: "fixture" },
      };
      await page.evaluate(
        (value) => window.accountErrorsFixture.changeError(value),
        changed,
      );
      limitsHttpFailure = true;
      await page.evaluate(() =>
        window.accountErrorsFixture.showSurface("accounts"),
      );
      const manager = page.locator(".accounts-manager-inline");
      await manager.waitFor();
      await details(changed, manager);
      const limits = manager.getByLabel("Limits for Fixture account");
      await limits
        .getByText("Provider request failed")
        .waitFor({ state: "attached" });
      await limits
        .getByRole("button", { name: "Error details", exact: true })
        .click();
      const httpDiagnostic: import("../../src/apiTypes.ts").Json = JSON.parse(
        await limits
          .getByRole("button", { name: "Hide error details", exact: true })
          .locator("..")
          .locator(":scope > span")
          .last()
          .innerText(),
      );
      assert.equal(httpDiagnostic.status, 503);
      assert.equal(httpDiagnostic.message, failure.message);
      assert.deepEqual(httpDiagnostic.location, limitsFailureBody.location);
      assert.equal(await page.locator("[data-shell]").count(), 1);
      limitsHttpFailure = false;
      limitsRecovered = true;
      const readsBeforeChange = limitsReads;
      const recoveredRead = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/limits" &&
          response.status() === 200,
        { timeout: 5000 },
      );
      assert.ok(
        [...resourceStreams].some((stream) =>
          stream.resources.some(
            (resource) =>
              resource.kind === "limits" && resource.accountKey === "default",
          ),
        ),
        "the active limits resource is subscribed",
      );
      for (const stream of resourceStreams) {
        stream.notify(
          stream.resources.filter(
            (resource) =>
              resource.kind === "limits" && resource.accountKey === "default",
          ),
        );
      }
      await recoveredRead;
      await limits
        .getByText(/80%\s*left/)
        .first()
        .waitFor();
      assert.equal(limitsReads, readsBeforeChange + 1);
      assert.equal(
        await limits
          .locator(".account-limit-summary")
          .getByText("80% left", { exact: true })
          .count(),
        1,
        "the account summary displays the recovered remaining limit",
      );
      assert.equal(
        await limits.getByText(/5h\s+80%\s+left/).count(),
        1,
        "typed notification displays the recovered primary limit",
      );
      assert.equal(
        await limits.getByText(/7d\s+80%\s+left/).count(),
        1,
        "typed notification displays the recovered weekly limit",
      );
      assert.equal(
        await limits.getByText(failure.message, { exact: true }).count(),
        0,
        "a successful typed resource reread clears the previous limit error",
      );
      const readsBeforeReconnect = limitsReads;
      const reconnectRead = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/limits" &&
          response.status() === 200,
        { timeout: 5000 },
      );
      for (const stream of resourceStreams)
        stream.notify(stream.resources, "reconnect");
      await reconnectRead;
      await limits
        .getByText(/80%\s*left/)
        .first()
        .waitFor();
      assert.equal(limitsReads, readsBeforeReconnect + 1);
      assert.equal(
        await limits
          .locator(".account-limit-summary")
          .getByText("80% left", { exact: true })
          .count(),
        1,
      );
      assert.equal(await limits.getByText(/5h\s+80%\s+left/).count(), 1);
      assert.equal(await limits.getByText(/7d\s+80%\s+left/).count(), 1);
      await page.keyboard.press("Escape");
      for (const section of ["rules", "changes", "tools"]) {
        await page.evaluate(
          (value) => window.accountErrorsFixture.showSurface(value),
          section,
        );
        if (section === "tools")
          await page
            .getByText("Some tools could not load. View details.", {
              exact: true,
            })
            .click();
        await page.getByText(failure.message, { exact: true }).waitFor();
        await details(failure);
      }
      assert.deepEqual(errors, []);
      assert.deepEqual(
        mutations,
        [],
        "Rendering error details never submits a form or repeats an operation",
      );
      console.log(
        `PASS ${browserType === chromium ? "Chromium" : "WebKit"}: sign-in, account, limits, rules, changes and tool diagnostics retain full structured fields without losing the shell`,
      );
    }
  } finally {
    await browser?.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
