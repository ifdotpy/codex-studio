import type { Route } from "@playwright/test";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
import type { ResourceRef } from "../playwright.ts";
import { required, fixturePostBody } from "../teams/helpers.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
// Actual TerminalDock input callbacks and HTTP, with an isolated xterm fixture.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.ts";

test("terminal-input-recovery-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const workspaceId = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  const identity = syncIdentityFixture(workspaceId);
  const temporary = await mkdtemp(
    join(tmpdir(), "studio-terminal-input-recovery-"),
  );
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: temporary,
    optimizeDeps: {
      include: [
        "react",
        "react-dom/client",
        "react/jsx-dev-runtime",
        "@mantine/core",
        "lucide-react",
      ],
    },
    plugins: [
      {
        name: "terminal-input-harness",
        enforce: "pre",
        resolveId(id) {
          if (id === "@xterm/xterm")
            return fileURLToPath(
              new URL("./terminal-stub.ts", import.meta.url),
            );
          if (id === "@xterm/addon-fit")
            return fileURLToPath(new URL("./fit-stub.ts", import.meta.url));
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = fixturePage;
    page.setDefaultTimeout(5000);
    const errors: string[] = [],
      posts: PostBody<"/api/terminals/input">[] = [],
      expected: string[] = [];
    const held = new Map<string, Route>(),
      receipts = new Map<
        string,
        {
          body: PostBody<"/api/terminals/input">;
          reply: PostResult<"/api/terminals/input">;
        }
      >(),
      holdTexts = new Set<string>();
    const failOutput = new Set<string>();
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: identity,
        headers: apiSchemaHeaders(),
      }),
    );
    await page.route("**/api/sync/protocol", (route) =>
      route.fulfill({
        json: syncProtocolFixture(),
        headers: apiSchemaHeaders(),
      }),
    );
    await page.route("**/api/sync/stream?*", (route) => {
      const resources: ResourceRef[] = JSON.parse(
        new URL(route.request().url()).searchParams.get("resources") || "[]",
      );
      return route.fulfill({
        contentType: "text/event-stream",
        headers: apiSchemaHeaders(),
        body: apiSchemaHandshakeSse(
          protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId,
            epoch: "terminal-input",
            revision: 1,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision: 1,
            })),
          }),
        ),
      });
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="app"></div>',
      }),
    );
    await page.route("**/api/terminals", (route) =>
      route.fulfill({
        json: {
          items: ["a", "b"].map((id) => ({
            id: "shell-" + id,
            agent: "agent",
            title: "Shell " + id.toUpperCase(),
            cwd: "/fixture",
            status: "running",
            created: 1,
          })),
        },
        headers: apiSchemaHeaders(),
      }),
    );
    await page.route(/\/api\/terminals\/output(?:\?.*)?$/, (route) => {
      const id = new URL(route.request().url()).searchParams.get("id");
      if (failOutput.delete(id ?? ""))
        return route.fulfill({
          status: 503,
          json: { error: "Output connection lost" },
          headers: apiSchemaHeaders(),
        });
      return route.fulfill({
        json: { text: "", offset: 0, truncated: false, status: "running" },
        headers: apiSchemaHeaders(),
      });
    });
    await page.route("**/api/terminals/resize", (route) =>
      route.fulfill({ json: { ok: true }, headers: apiSchemaHeaders() }),
    );
    await page.route("**/api/terminals/input", (route) => {
      const body = fixturePostBody(route, "/api/terminals/input");
      assert.equal(
        route.request().headers()["x-canvas-token"],
        "fixture-token",
      );
      assert.ok(body.request_id);
      posts.push(body);
      const previous = receipts.get(body.request_id);
      if (previous) {
        assert.deepEqual(body, previous.body);
        return route.fulfill({
          json: previous.reply,
          headers: apiSchemaHeaders(),
        });
      }
      // One native effect happens before the fixture can lose its HTTP response.
      receipts.set(body.request_id, {
        body,
        reply: { ok: false, delivery: "uncertain" },
      });
      if (holdTexts.has(body.text)) {
        held.set(body.text, route);
        return;
      }
      return route.fulfill({
        json: { ok: true, delivery: "sent" },
        headers: apiSchemaHeaders(),
      });
    });
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
    await page.evaluate(async () => {
      window.terminalInputRecoveryTerminals = [];
      window.terminalInputRecoveryNotices = [];
      window.terminalInputRecoveryAdvance = 0;
      const now = Date.now;
      Date.now = () => now() + window.terminalInputRecoveryAdvance;
    });
    await page.addScriptTag({
      type: "module",
      url: "/tests/terminal/terminal-input-fixture.ts",
    });
    await page.waitForFunction(
      () => typeof window.terminalInputRecoveryMount === "function",
    );
    const until = async (
      check: () => boolean | Promise<boolean>,
      message: string,
    ) => {
      const deadline = Date.now() + 5000;
      while (!(await check())) {
        assert.ok(Date.now() < deadline, message);
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    };
    const count = () =>
      page.evaluate(() => window.terminalInputRecoveryTerminals.length);
    const mount = async () => {
      const before = await count();
      await page.evaluate(() => window.terminalInputRecoveryMount("shell-a"));
      await page.waitForFunction(
        (before) =>
          window.terminalInputRecoveryTerminals.length > before &&
          window.terminalInputRecoveryCurrentTerminal.input,
        before,
      );
      assert.equal(
        await page.evaluate(
          () =>
            window.terminalInputRecoveryCurrentTerminal.options.disableStdin,
        ),
        false,
      );
    };
    const input = (text: string) =>
      page.evaluate(
        (text) => window.terminalInputRecoveryCurrentTerminal.input?.(text),
        text,
      );
    const observed = (text: string) =>
      until(
        () => posts.some((post) => post.text === text),
        `Input ${text} must start`,
      );
    const accepted = async (text: string, hold = false) => {
      expected.push(text);
      if (hold) holdTexts.add(text);
      await input(text);
      await observed(text);
    };
    const reply = async (text: string, outcome = "success") => {
      const route = required(held.get(text));
      assert.ok(route, `Expected held input ${text}`);
      const body = fixturePostBody(route, "/api/terminals/input");
      const response: PostResult<"/api/terminals/input"> =
        outcome === "uncertain"
          ? {
              ok: false,
              delivery: "uncertain",
              error: "Late input outcome unknown",
            }
          : { ok: true, delivery: "sent" };
      required(receipts.get(body.request_id)).reply = response;
      await route.fulfill(
        outcome === "error"
          ? {
              status: 503,
              json: { error: "Late old input error" },
              headers: apiSchemaHeaders(),
            }
          : { json: response, headers: apiSchemaHeaders() },
      );
      held.delete(text);
      await page.waitForTimeout(50);
    };
    const assertUnpaused = async () => {
      assert.equal(
        await page.evaluate(
          () =>
            window.terminalInputRecoveryCurrentTerminal.options.disableStdin,
        ),
        false,
      );
      assert.equal(
        await page
          .getByText(/Input paused\. Reconnect before you continue\./)
          .count(),
        0,
      );
    };
    const reconnect = async (shell = "shell-a") => {
      const before = await count();
      failOutput.add(shell);
      await page
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await page.waitForFunction(
        (before) =>
          window.terminalInputRecoveryTerminals.length > before &&
          window.terminalInputRecoveryCurrentTerminal.input,
        before,
      );
      await assertUnpaused();
    };

    // Healthy input stays ordered behind one exact in-flight command.
    await mount();
    await accepted("healthy-1", true);
    expected.push("healthy-2");
    await input("healthy-2");
    await page.waitForTimeout(50);
    assert.deepEqual(
      posts.map((post) => post.text),
      ["healthy-1"],
    );
    await reply("healthy-1");
    await observed("healthy-2");
    assert.deepEqual(
      posts.map((post) => post.text),
      ["healthy-1", "healthy-2"],
    );

    // The original hang: a lost response expires after a suspended page resumes.
    await mount();
    await accepted("timeout-old", true);
    await input("discard-timeout");
    await page.evaluate(() => {
      window.terminalInputRecoveryAdvance += 16000;
      window.dispatchEvent(new Event("pageshow"));
    });
    await page
      .getByText(/Input paused\. Reconnect before you continue\./)
      .waitFor();
    assert.equal(
      await page.evaluate(
        () => window.terminalInputRecoveryCurrentTerminal.options.disableStdin,
      ),
      true,
    );
    await page.getByText(/Previous input delivery is uncertain\./).waitFor();
    await input("discard-while-paused");
    const beforeTimeoutReconnect = await count();
    await page.getByRole("button", { name: "Reconnect", exact: true }).click();
    await page.waitForFunction(
      (before) =>
        window.terminalInputRecoveryTerminals.length > before &&
        window.terminalInputRecoveryCurrentTerminal.input,
      beforeTimeoutReconnect,
    );
    await assertUnpaused();
    await page.getByText(/Previous input delivery is uncertain\./).waitFor();
    await accepted("timeout-fresh");
    await reply("timeout-old");

    // The stalled write must time out before the reconnect control can appear.
    // Reconnect then fences every late old outcome and queued callback.
    for (const outcome of ["success", "error", "uncertain"]) {
      await mount();
      await accepted(`${outcome}-old`, true);
      await input(`discard-${outcome}`);
      await page.evaluate(() => {
        window.terminalInputRecoveryAdvance += 16000;
        window.dispatchEvent(new Event("pageshow"));
      });
      await page
        .getByText(/Input paused\. Reconnect before you continue\./)
        .waitFor();
      await page.evaluate(
        () =>
          (window.terminalInputRecoveryRetiredTerminal =
            window.terminalInputRecoveryCurrentTerminal),
      );
      await reconnect();
      await page.getByText(/Previous input delivery is uncertain\./).waitFor();
      await page.evaluate(() =>
        window.terminalInputRecoveryRetiredTerminal.input?.(
          "discard-retired-callback",
        ),
      );
      await accepted(`${outcome}-fresh`, true);
      await reply(`${outcome}-old`, outcome);
      await assertUnpaused();
      expected.push(`${outcome}-fresh-2`);
      await input(`${outcome}-fresh-2`);
      assert.equal(
        posts.filter((post) => post.text === `${outcome}-fresh-2`).length,
        0,
      );
      await reply(`${outcome}-fresh`);
      await observed(`${outcome}-fresh-2`);
      await page.getByText(/Previous input delivery is uncertain\./).waitFor();
    }

    // Unmount prevents old queued keys and late errors from reaching a new view.
    await mount();
    await accepted("unmount-old", true);
    await input("discard-unmount");
    await page.evaluate(() => {
      window.terminalInputRecoveryRetiredTerminal =
        window.terminalInputRecoveryCurrentTerminal;
      window.terminalInputRecoveryRoot?.unmount();
      window.terminalInputRecoveryRoot = null;
    });
    await mount();
    await accepted("unmount-fresh", true);
    await reply("unmount-old", "error");
    await page.evaluate(() =>
      window.terminalInputRecoveryRetiredTerminal.input?.(
        "discard-unmounted-callback",
      ),
    );
    await assertUnpaused();
    await reply("unmount-fresh");

    // Switching shells uses the new shell identity and a separate input chain.
    await mount();
    await accepted("switch-old", true);
    await input("discard-switch");
    const beforeSwitch = await count();
    await page
      .locator(".terminal-session")
      .filter({ hasText: "Shell B" })
      .click();
    await page.waitForFunction(
      (before) =>
        window.terminalInputRecoveryTerminals.length > before &&
        window.terminalInputRecoveryCurrentTerminal.input,
      beforeSwitch,
    );
    await accepted("switch-fresh", true);
    assert.equal(
      required(posts.find((post) => post.text === "switch-fresh")).id,
      "shell-b",
    );
    await reply("switch-old");
    await assertUnpaused();
    await reply("switch-fresh");

    assert.deepEqual(
      posts.map((post) => post.text),
      expected,
    );
    assert.equal(
      new Set(posts.map((post) => post.request_id)).size,
      posts.length,
    );
    assert.equal(
      receipts.size,
      expected.length,
      "Every intended input has one logical native effect",
    );
    expect(errors).toEqual([]);
    console.log(
      "PASS: ordered input, resume deadline, manual reconnect, late outcomes, unmount, shell switch, and no stale input replay",
    );
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
