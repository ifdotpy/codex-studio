import type { BrowserContext, TestInfo } from "@playwright/test";
import type { components } from "../../src/generated/api.ts";
import type { PostBody, PostResult } from "../../src/api.ts";
import { fixtureUrl } from "./fixture-helpers.ts";
import type {} from "./claude-settings-auth-ui.fixture.tsx";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";

import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
} from "../playwright.ts";

const browserContextsByTest = new WeakMap<TestInfo, Set<BrowserContext>>();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("claude settings auth ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const root = resolve(import.meta.dirname, "../../");
  const workspaceId = "0123456789abcdef0123456789abcdef";
  const { createServer } = await import("vite");
  const cacheDir = await mkdtemp(join(tmpdir(), "claude-settings-auth-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/settings/claude-settings-auth-ui.fixture.tsx"></script>',
            );
          });
          server.middlewares.use("/api/sync/stream", (req, res) => {
            const resources: components["schemas"]["ResourceRef"][] =
              JSON.parse(
                new URL(req.url || "/", "http://localhost").searchParams.get(
                  "resources",
                ) || "[]",
              );
            res.writeHead(200, {
              "Content-Type": "text/event-stream",
              "Cache-Control": "no-cache",
              Connection: "keep-alive",
              ...apiSchemaHeaders(),
            });
            res.write(
              apiSchemaHandshakeSse(
                protocol3SseEvent("resources", {
                  protocol: 4,
                  workspaceId,
                  epoch: "claude-settings-auth",
                  revision: 1,
                  reason: "initial",
                  resources,
                  resourceVersions: resources.map((resource) => ({
                    resource,
                    revision: 1,
                  })),
                }),
              ),
            );
            const heartbeat = setInterval(() => {
              res.write(
                `event: heartbeat\ndata: ${JSON.stringify({ protocol: 4, workspaceId, epoch: "claude-settings-auth", revision: 1 })}\n\n`,
              );
            }, 1000);
            res.on("close", () => clearInterval(heartbeat));
          });
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = _browser;
    const page = await browser.newPage({
      viewport: { width: 390, height: 844 },
    });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let authenticated = false;
    let receipt: PostResult<"/api/accounts/claude/login"> | undefined;
    let settingsReads = 0;
    let modelReads = 0;
    const loginStarts: PostBody<"/api/accounts/claude/login">[] = [];
    const mutations: PostBody<"/api/claude/session">[] = [];
    await page.route("**/api/**", async (route) => {
      const req = route.request();
      const url = new URL(req.url());

      if (url.pathname === "/api/claude/session") {
        const body: PostBody<"/api/claude/session"> = req.postDataJSON();
        assert.equal(body.id, "chat");
        if (body.action !== "state") mutations.push(body);
        else settingsReads++;
        if (!authenticated)
          return route.fulfill({
            status: 400,
            json: { error: "Sign in with claude auth login first" },
          });
        return route.fulfill({
          json: {
            settings: { permissionMode: "acceptEdits", thinking: true },
            turns: [],
          },
        });
      }
      if (url.pathname === "/api/models") {
        modelReads++;
        assert.equal(authenticated, true);
        assert.equal(url.searchParams.get("account_key"), "claude-work");
        return route.fulfill({ json: { data: [{ model: "sonnet" }] } });
      }
      if (url.pathname === "/api/accounts/claude/login") {
        if (req.method() === "POST") {
          const body: PostBody<"/api/accounts/claude/login"> =
            req.postDataJSON();
          loginStarts.push(body);
          receipt = {
            requestId: body.login_id,
            accountKey: body.account_key,
            status: "pending",
            verificationUrl: "https://claude.ai/oauth/authorize?fixture=1",
          };
          return route.fulfill({ json: receipt });
        }
        const status = receipt || {
          requestId: url.searchParams.get("request_id"),
          accountKey: "claude-work",
          status: "starting",
        };
        assert.equal(status.requestId, url.searchParams.get("request_id"));
        assert.equal(status.accountKey, "claude-work");
        return route.fulfill({ json: status });
      }
      if (url.pathname === "/api/accounts/claude/login/code") {
        authenticated = true;
        assert.ok(receipt);
        receipt = { ...receipt, status: "ready", email: "work@example.com" };
        return route.fulfill({ json: receipt });
      }
      return route.fulfill({ json: {} });
    });
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: syncIdentityFixture(workspaceId),
        headers: apiSchemaHeaders(),
      }),
    );
    await page.route("**/api/sync/protocol", (route) =>
      route.fulfill({
        json: syncProtocolFixture(),
        headers: apiSchemaHeaders(),
      }),
    );
    // The broad API fixture below must not turn the event stream into JSON.
    await page.route("**/api/sync/stream?*", (route) => route.fallback());
    await page.goto(fixtureUrl(server, "check"));
    await page
      .getByRole("alert")
      .filter({ hasText: "Sign in to Claude to load these settings." })
      .waitFor();
    assert.equal(await page.getByLabel("Permission mode").isDisabled(), true);
    assert.equal(await page.getByLabel("Extended thinking").isDisabled(), true);
    assert.equal(modelReads, 0);
    await page.getByText("Claude needs sign-in. work@example.com").waitFor();
    await page
      .getByRole("button", { name: "Sign in to Claude", exact: true })
      .click();
    assert.equal(
      await page.evaluate(() => window.claudeSettingsAuthFixture.signInAccount),
      "claude-work",
    );
    await page
      .getByRole("button", { name: "Start sign-in", exact: true })
      .click();
    await page.getByRole("link", { name: "Open Claude sign-in" }).waitFor();
    assert.equal(loginStarts.length, 1);
    assert.equal(loginStarts.at(0)?.account_key, "claude-work");
    await page.getByLabel("Final authorization response").fill("fixture-code");
    await page
      .getByRole("button", { name: "Submit authorization response" })
      .click();
    await page.waitForFunction(
      () =>
        !document.querySelector<HTMLSelectElement>(
          'select[aria-label="Permission mode"]',
        )?.disabled &&
        [...document.querySelectorAll("select")].some(
          (select) => select.value === "acceptEdits" && !select.disabled,
        ),
    );
    assert.equal(
      await page.getByLabel("Permission mode").inputValue(),
      "acceptEdits",
    );
    await page.waitForFunction(() =>
      [
        ...document.querySelectorAll<HTMLInputElement>(
          'input[type="checkbox"]',
        ),
      ].some((input) => !input.disabled),
    );
    assert.equal(
      await page.getByLabel("Extended thinking").isDisabled(),
      false,
    );
    assert.equal(
      await page
        .getByRole("button", { name: "Sign in to Claude", exact: true })
        .count(),
      0,
    );
    assert.equal(settingsReads, 2);
    assert.equal(modelReads, 1);
    assert.deepEqual(mutations, []);
    assert.deepEqual(errors, []);

    // A chat without a native thread can return stored settings after sign-out.
    const emptyChat = await browser.newPage();
    const emptyChatRequests: PostBody<"/api/claude/session">[] = [];
    await emptyChat.route("**/api/**", async (route) => {
      const req = route.request();
      const url = new URL(req.url());
      if (url.pathname === "/api/claude/session") {
        emptyChatRequests.push(req.postDataJSON());
        return route.fulfill({
          json: {
            settings: { permissionMode: "acceptEdits", thinking: true },
            turns: [],
          },
        });
      }
      assert.notEqual(url.pathname, "/api/models");
      return route.fulfill({ json: {} });
    });
    await emptyChat.goto(fixtureUrl(server, "check"));
    await emptyChat.waitForFunction(
      () =>
        document
          .querySelector('[aria-label="Claude settings"]')
          ?.getAttribute("aria-busy") === "false",
    );
    await emptyChat
      .getByText("Claude needs sign-in. work@example.com")
      .waitFor();
    assert.equal(
      await emptyChat.getByLabel("Permission mode").inputValue(),
      "acceptEdits",
    );
    assert.equal(
      await emptyChat.getByLabel("Permission mode").isDisabled(),
      true,
    );
    assert.equal(
      await emptyChat.getByLabel("Extended thinking").isDisabled(),
      true,
    );
    await emptyChat
      .getByText("More settings and actions", { exact: true })
      .click();
    assert.equal(
      await emptyChat.getByLabel("Auto-compact token limit").isDisabled(),
      true,
    );
    assert.deepEqual(
      emptyChatRequests.map((request) => request.action),
      ["state"],
    );
    await emptyChat.close();
    console.log(
      "PASS Claude settings sign-out, pinned account login, disabled writes, model load and settings reload after sign-in",
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
