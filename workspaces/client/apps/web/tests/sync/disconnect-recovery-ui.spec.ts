import type { Route } from "@playwright/test";
import type { Agent } from "../../src/types.ts";
import type { PostResult } from "../../src/apiTypes.ts";
import {
  original,
  disconnected,
  restarted,
} from "./disconnect-recovery-ui.data.ts";
import { fixtureOrigin } from "./helpers.ts";
import type {} from "./disconnect-recovery-ui.fixture.tsx";
// Real React with a fake recovery API. No backend, credentials, or model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { test, expect } from "../playwright.ts";

test("disconnect recovery ui", async ({ page: runnerPage }) => {
  const root = resolve(import.meta.dirname, "../../");
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const evidence = await mkdtemp(join(tmpdir(), "studio-disconnect-recovery-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(evidence, "vite"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "disconnect-recovery-fixture",
        configureServer(server) {
          server.middlewares.use(
            "/disconnect-recovery-test",
            (_request, response) => {
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<html><body><div id="root"></div><script type="module" src="/tests/sync/disconnect-recovery-ui.fixture.tsx"></script></body></html>',
              );
            },
          );
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    const errors: string[] = [];
    const requests: {
      path: string;
      method: string;
      body: unknown;
      token: string | undefined;
    }[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let reply: PostResult<"/api/connection-recovery"> = {
      status: "unconfirmed",
    };
    const held: { route?: Route | undefined } = {};
    const heldRoute = () => {
      const route = held.route;
      if (!route) throw new Error("Missing held recovery route");
      return route;
    };
    let hold = false,
      fail = false;
    await page.route("**/api/**", async (route) => {
      const request = route.request();
      requests.push({
        path: new URL(request.url()).pathname,
        method: request.method(),
        body: request.postDataJSON(),
        token: request.headers()["x-canvas-token"],
      });
      if (hold) {
        held.route = route;
        return;
      }
      await route.fulfill(
        fail
          ? { status: 503, json: { error: "Recovery service unavailable" } }
          : { json: reply },
      );
    });
    await page.goto(fixtureOrigin(server) + "/disconnect-recovery-test");
    const button = page.getByRole("button", {
      name: "Check connection",
      exact: true,
    });
    await button.waitFor();
    const history = await page.locator("[data-history]").elementHandle();
    assert.ok(history);
    assert.equal(
      requests.length,
      0,
      "mounting the notice does not start recovery",
    );
    hold = true;
    await button.evaluate((node) => {
      if (!(node instanceof HTMLElement))
        throw new Error("Missing recovery button");
      node.click();
      node.click();
    });
    await page.waitForFunction(
      () => document.querySelector("button")?.disabled,
    );
    for (let attempt = 0; !held.route && attempt < 100; attempt++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert.ok(held.route);
    assert.equal(
      requests.length,
      1,
      "duplicate click cannot start another recovery check",
    );
    await heldRoute().fulfill({ json: reply });
    hold = false;
    held.route = undefined;
    await page
      .getByRole("status")
      .filter({ hasText: "previous outcome remains unconfirmed" })
      .waitFor();
    assert.match(
      await page.locator(".native-error").innerText(),
      /Codex disconnected/,
    );
    assert.equal(await history.evaluate((node) => node.isConnected), true);
    assert.deepEqual(
      await page.evaluate(() => window.disconnectRecoveryUiFixtureRefreshes),
      [],
    );

    for (const outcome of ["completed", "failed", "interrupted"] as const) {
      reply = { status: "reconciled", outcome };
      await button.click();
      await page.waitForFunction(
        (count) => window.disconnectRecoveryUiFixtureRefreshes.length === count,
        ["completed", "failed", "interrupted"].indexOf(outcome) + 1,
      );
    }
    reply = { status: "superseded" };
    await button.click();
    await page
      .getByRole("status")
      .filter({ hasText: "conversation changed" })
      .waitFor();
    assert.equal(
      await page.evaluate(
        () => window.disconnectRecoveryUiFixtureRefreshes.length,
      ),
      3,
    );
    fail = true;
    await button.click();
    await page
      .getByRole("status")
      .filter({ hasText: "Could not finish the connection check." })
      .waitFor();
    await page.locator(".studio-recovery-banner summary").click();
    await page
      .locator(".studio-recovery-banner")
      .getByText("Recovery service unavailable", { exact: false })
      .waitFor();
    fail = false;
    assert.match(
      await page.locator(".native-error").innerText(),
      /Codex disconnected/,
    );

    for (const patch of [
      { status: "running" },
      { inFlight: true },
      { threadId: null },
      { turnId: null },
      { error: "Codex disconnected." },
      { error: "An unrelated interruption" },
      { error: { message: disconnected } },
      // @ts-expect-error negative eligibility case: imported is an unsupported source
      { source: "imported" },
      { autoWake: true },
      // @ts-expect-error negative eligibility case: legacy startAttempt.id is not a current receipt
      { startAttempt: { id: "starting" } },
      { accountTransferId: "transfer" },
      { workspaceOperation: "restore" },
      { deletedAt: 1 },
      {
        nativeThreadBlock: {
          threadId: "thread-first",
          error: { codexErrorInfo: "misalignmentPolicyViolation" },
        },
      },
    ] satisfies Partial<Agent>[]) {
      await page.evaluate(
        // @ts-expect-error negative eligibility inputs include unsupported source and legacy startAttempt.id
        (agent) => window.disconnectRecoveryUiFixtureSetAgent(agent),
        {
          ...original,
          ...patch,
        },
      );
      await button.waitFor({ state: "hidden" });
    }
    await page.evaluate(
      (agent) => window.disconnectRecoveryUiFixtureSetAgent(agent),
      {
        ...original,
        error: restarted,
      },
    );
    await button.waitFor();
    reply = {
      status: "unconfirmed",
      error: "Native history remains unavailable.",
    };
    await button.click();
    await page
      .getByRole("status")
      .filter({ hasText: "Could not confirm the previous outcome." })
      .waitFor();
    await page.locator(".studio-recovery-banner summary").click();
    await page
      .locator(".studio-recovery-banner")
      .getByText("Native history remains unavailable.", { exact: true })
      .waitFor();
    assert.doesNotMatch(
      await page.getByRole("status").innerText(),
      /Connection checked/,
      "a failed transport or history read cannot claim a successful connection check",
    );
    assert.match(
      await page.locator(".native-error").innerText(),
      /Server restarted during a turn/,
    );

    hold = true;
    await button.click();
    for (let attempt = 0; !held.route && attempt < 100; attempt++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert.ok(held.route);
    await page.evaluate(
      (agent) => window.disconnectRecoveryUiFixtureSetAgent(agent),
      {
        ...original,
        id: "second",
        threadId: "thread-second",
        turnId: "turn-second",
      },
    );
    await page.locator('[data-agent="second"]').waitFor();
    await heldRoute().fulfill({
      json: { status: "reconciled", outcome: "completed" },
    });
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    assert.deepEqual(
      await page.evaluate(() => window.disconnectRecoveryUiFixtureRefreshes),
      ["first", "first", "first"],
      "an old chat response cannot refresh a different chat",
    );
    assert.equal(
      await page.getByRole("status").count(),
      0,
      "old recovery text cannot leak into another chat",
    );
    assert.equal(await button.isEnabled(), true);
    hold = false;
    held.route = undefined;
    reply = { status: "unconfirmed", checked: true };
    await page.evaluate(
      (agent) => window.disconnectRecoveryUiFixtureSetAgent(agent),
      original,
    );
    await button.click();
    await page.waitForFunction(
      () => window.disconnectRecoveryUiFixtureRefreshes.length === 4,
    );
    const checkedAgent = {
      ...original,
      connectionCheck: {
        at: Date.now() / 1000,
        epoch: original.epoch,
        accountKey: original.accountKey,
        threadId: original.threadId,
        turnId: original.turnId,
        previousError: original.error,
        nativeState: "idle",
      },
    } satisfies Agent;
    await page.evaluate((agent) => {
      localStorage.setItem("fixture-agent", JSON.stringify(agent));
      window.disconnectRecoveryUiFixtureSetAgent(agent);
    }, checkedAgent);
    const reviewTitle = page.getByText("Previous turn needs review", {
      exact: true,
    });
    await reviewTitle.waitFor();
    await page
      .getByText(
        "Codex answered the connection check. The previous outcome is unconfirmed. Review the history before continuing.",
        { exact: true },
      )
      .waitFor();
    assert.doesNotMatch(
      await page.locator(".native-error > span").innerText(),
      /disconnected/,
    );
    await page.locator(".native-error details > summary").click();
    assert.equal(
      await page.locator(".native-error details pre").innerText(),
      disconnected,
    );
    const beforeReload = requests.length;
    await page.reload();
    await reviewTitle.waitFor();
    assert.equal(
      requests.length,
      beforeReload,
      "a saved receipt restores presentation without another check",
    );
    await button.waitFor();
    for (const patch of [
      { accountKey: "other" },
      { epoch: 2 },
      { threadId: "other-thread" },
      { turnId: "other-turn" },
      { error: restarted },
      { status: "completed" },
      {
        connectionCheck: {
          ...checkedAgent.connectionCheck,
          previousError: restarted,
        },
      },
      { connectionCheck: { ...checkedAgent.connectionCheck, at: 0 } },
    ] satisfies Partial<Agent>[]) {
      await page.evaluate(
        (agent) => window.disconnectRecoveryUiFixtureSetAgent(agent),
        {
          ...checkedAgent,
          ...patch,
        },
      );
      await reviewTitle.waitFor({ state: "hidden" });
    }
    await page.evaluate(
      (agent) => window.disconnectRecoveryUiFixtureSetAgent(agent),
      {
        ...checkedAgent,
        connectionCheck: {
          ...checkedAgent.connectionCheck,
          nativeState: "active",
        },
      },
    );
    await reviewTitle.waitFor();
    await page
      .getByText(
        "Codex reported this thread as active. Its outcome is not confirmed.",
        { exact: true },
      )
      .waitFor();
    assert.equal(
      await page.locator("[data-history]").innerText(),
      "Previously recorded conversation remains available.",
    );
    assert.ok(
      requests.every(
        (request) =>
          request.path === "/api/connection-recovery" &&
          request.method === "POST" &&
          request.token === "fixture-token",
      ),
    );
    assert.ok(
      requests.every(
        (request) =>
          JSON.stringify(request.body) === JSON.stringify({ id: "first" }),
      ),
      "the only mutation checks connection; no prompt is sent",
    );
    expect(errors).toEqual([]);
    await page.screenshot({ path: join(evidence, "disconnect-recovery.png") });
    console.log(
      JSON.stringify({
        result: "PASS",
        cases: [
          "exact eligibility",
          "explicit click",
          "duplicate lock",
          "reconciled outcomes",
          "unconfirmed history retained",
          "superseded",
          "request failure",
          "stale chat response",
          "saved checked receipt after reload",
          "receipt identity guards",
          "active native thread stays unconfirmed",
          "no prompt",
        ],
        evidence,
      }),
    );
  } finally {
    await server.close();
  }
});
