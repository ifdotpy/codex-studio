import type { PostBody, GetResult } from "../../src/apiTypes.ts";
import { required, fixturePostBody } from "../teams/helpers.ts";
import { test, expect } from "../playwright.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test.describe.configure({ mode: "parallel" });

// A headless fixture mounts the production Workspace Tools caller. The API
// fixture controls delayed replies, response loss and inventory pagination.
for (const width of [320, 1200]) {
  test(`MCP reconnect in Workspace at ${width}px`, async ({
    page,
  }, testInfo) => {
    const root = resolve(import.meta.dirname, "../..");
    const cacheDir = await mkdtemp(join(tmpdir(), "mcp-reconnect-ui-"));
    const server = await createServer({
      configFile: false,
      root,
      cacheDir,
      server: { host: "127.0.0.1", port: 0, hmr: false },
      plugins: [
        {
          name: "mcp-reconnect-fixture",
          configureServer(server) {
            server.middlewares.use("/check", (_req, res) => {
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/workspace/mcp-reconnect-fixture.tsx"></script>',
              );
            });
          },
        },
      ],
    });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const calls: PostBody<"/api/mcp/reconnect">[] = [];
    const pageReads: (string | null)[] = [];
    let connected = false;
    let thread = "thread";
    let release = () => {};
    const delayed = new Promise<void>((resolve) => {
      release = resolve;
    });
    let mode = "delayed";
    const inventory = (
      cursor: string | null,
    ): GetResult<"/api/capabilities"> => ({
      agent: "agent",
      at: 1,
      model: "model",
      role: "orchestrator",
      managed: [],
      observed: [],
      observedNative: [],
      nativeInventory: "No recorded calls.",
      skills: [],
      servers: [],
      errors: [],
      mcpSession: {
        account: "account",
        thread,
        epoch: 2,
        connection: "connection",
        generation: 1,
      },
      mcp: cursor
        ? [{ name: "second-server", status: "connected" }]
        : [
            {
              name: "server-with-a-long-name-for-small-window-layout-check",
              status: connected ? "connected" : "failed",
              error: connected
                ? null
                : "The MCP endpoint refused the connection. Reconnect this server to try again.",
            },
          ],
      serversCursor: cursor ? null : "next",
    });
    try {
      await server.listen();
      await page.setViewportSize({ width, height: 1000 });
      await page.route("**/api/capabilities?*", (route) => {
        const cursor = new URL(route.request().url()).searchParams.get(
          "serversCursor",
        );
        pageReads.push(cursor);
        return route.fulfill({ json: inventory(cursor) });
      });
      await page.route("**/api/mcp/reconnect", async (route) => {
        calls.push(fixturePostBody(route, "/api/mcp/reconnect"));
        if (mode === "delayed") await delayed;
        if (mode === "lost") {
          mode = "replay";
          return route.abort("failed");
        }
        if (mode === "codex")
          return route.fulfill({
            status: 400,
            json: { error: "MCP server reconnect is not supported for Codex." },
          });
        connected = true;
        return route.fulfill({
          json: { status: "completed", message: "Reconnect completed." },
        });
      });
      await page.goto(required(server.resolvedUrls?.local[0]) + "check");
      const first = page.getByRole("region", {
        name: "MCP server server-with-a-long-name-for-small-window-layout-check",
        exact: true,
      });
      await expect(first).toContainText("Status: failed");
      await page.getByRole("button", { name: "Load more servers" }).click();
      await expect(
        page.getByRole("region", {
          name: "MCP server second-server",
          exact: true,
        }),
      ).toBeVisible();
      expect(pageReads).toContain("next");
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(first).toContainText("Reconnect in progress.");
      await expect(first.getByRole("button")).toBeDisabled();
      release();
      await expect(first).toContainText("Reconnect completed.");
      await expect(first).toContainText("Status: connected");
      expect(required(calls[0]).session.thread).toBe("thread");
      expect(required(calls[0]).name).toContain("server-with-a-long-name");
      mode = "lost";
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(
        first.getByRole("button", { name: "Check reconnect result" }),
      ).toBeEnabled();
      await page.reload();
      await first
        .getByRole("button", { name: "Check reconnect result", exact: true })
        .click();
      await expect(first).toContainText("Reconnect completed.");
      expect(required(calls[2]).request_id).toBe(required(calls[1]).request_id);
      mode = "codex";
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(first).toContainText("not supported for Codex");
      await first.scrollIntoViewIfNeeded();
      const box = required(await first.boundingBox());
      expect(box.x).toBeGreaterThanOrEqual(0);
      expect(box.x + box.width).toBeLessThanOrEqual(width);
      expect(
        await first.evaluate((node) => node.scrollWidth <= node.clientWidth),
      ).toBe(true);
      await page.screenshot({
        path: testInfo.outputPath(`mcp-reconnect-${width}.png`),
        fullPage: true,
      });
      thread = "new-thread";
      await page
        .getByRole("button", { name: "Refresh servers", exact: true })
        .click();
      await expect(
        first.getByRole("button", { name: "Reconnect", exact: true }),
      ).toBeEnabled();
      mode = "replay";
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(first).toContainText("Reconnect completed.");
      expect(required(calls.at(-1)).session.thread).toBe("new-thread");
      expect(required(calls.at(-1)).request_id).not.toBe(
        required(calls.at(-2)).request_id,
      );
      expect(errors).toEqual([]);
    } finally {
      release();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}
