import type {} from "./team-room-bounds-ui.fixture.tsx";
import { required } from "./helpers.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.ts";

test("team room bounds ui", async ({ page: runnerPage }) => {
  const root = resolve(import.meta.dirname, "../../");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-room-bounds-"));
  const workspaceId = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  const identity = syncIdentityFixture(workspaceId);
  const room = {
    id: "private:lead:worker",
    kind: "private",
    members: ["lead", "worker"],
    name: "Worker",
  };
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "room-bounds-fixture",
        configureServer(vite) {
          vite.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<html><body><div id="root"></div><script type="module" src="/tests/teams/team-room-bounds-ui.fixture.tsx"></script></body></html>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 900, height: 620 });
    page.setDefaultTimeout(10000);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const requests: { before: number | null; after: number | null }[] = [];
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
      const resources = JSON.parse(
        new URL(route.request().url()).searchParams.get("resources") || "[]",
      );
      return route.fulfill({
        contentType: "text/event-stream",
        headers: apiSchemaHeaders(),
        body: apiSchemaHandshakeSse(
          protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId,
            epoch: "room-bounds",
            revision: 1,
            reason: "initial",
            resources,
            // Force the mounted room query to pull its first message page.
            resourceVersions: [],
          }),
        ),
      });
    });
    await page.route("**/api/agent-chat?*", async (route) => {
      const url = new URL(route.request().url());
      const before = Number(url.searchParams.get("before"));
      const after = Number(url.searchParams.get("after"));
      requests.push({ before: before || null, after: after || null });
      let lo = 1292,
        hi = 1391;
      if (before) {
        hi = before - 1;
        lo = Math.max(1, hi - 99);
      } else if (after) {
        lo = after + 1;
        hi = Math.min(1391, lo + 99);
      }
      const messages = [];
      for (let seq = lo; seq <= hi; seq++)
        messages.push({
          id: `m${seq}`,
          seq,
          room: "private:lead:worker",
          sender: "worker",
          senderName: "Worker",
          text: `room message ${seq} ` + "x".repeat(120),
          created: seq,
          deliveries: {},
        });
      return route.fulfill({
        headers: apiSchemaHeaders(),
        json: {
          room,
          messages,
          nextBefore: lo > 1 ? lo : null,
          nextAfter: hi < 1391 ? hi : null,
        },
      });
    });
    await page.goto(`${required(server.resolvedUrls?.local[0])}check`);
    await page.locator('[data-room="private:lead:worker"]').click();
    const start = Date.now();
    await page
      .locator(".team-message")
      .filter({ hasText: "room message 1391" })
      .waitFor();
    const initialMs = Date.now() - start;
    const scroll = page.locator(".unified-message-scroll");
    await page.getByRole("button", { name: "Earlier messages" }).click();
    await page.getByRole("button", { name: "Earlier messages" }).click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLElement>(".unified-message-scroll")?.dataset
          .roomRetained === "240",
    );
    const retained = await scroll.getAttribute("data-room-retained");
    const mounted = await page.locator(".team-message").count();
    assert.equal(retained, "240");
    assert.ok(mounted < 60, `mounted ${mounted} messages`);
    assert.ok(mounted > 0);
    assert.ok(
      await page.getByRole("button", { name: "Later messages" }).count(),
    );
    await page.getByRole("button", { name: "Later messages" }).click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLElement>(".unified-message-scroll")?.dataset
          .roomRetained === "240",
    );
    assert.ok(requests.some((request) => request.before !== null));
    assert.ok(requests.some((request) => request.after !== null));
    await page
      .getByRole("button", { name: "Latest messages", exact: true })
      .click();
    await expect(page.locator('[data-message="m1391"]')).toBeVisible();
    const bottomGap = () =>
      scroll.evaluate(
        (element) =>
          element.scrollHeight - element.scrollTop - element.clientHeight,
      );
    await expect.poll(bottomGap).toBeLessThan(2);
    // A mounted row grows after its first measurement (for example, an image loads).
    await page.locator('[data-message="m1391"]').evaluate((element) => {
      element.style.paddingBottom = "300px";
    });
    await expect.poll(bottomGap).toBeLessThan(2);
    expect(await page.locator(".team-message").count()).toBeLessThan(60);
    expect(errors).toEqual([]);
    console.log(
      JSON.stringify({
        ok: true,
        retainedMessages: Number(retained),
        mountedMessages: mounted,
        initialRenderMs: initialMs,
        requests,
      }),
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
