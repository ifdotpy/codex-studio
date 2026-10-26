import type { PostBody } from "../../src/apiTypes.ts";
import { required, fixturePostBody } from "../teams/helpers.ts";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("Claude checkpoints show the reason and send one combined restore identity", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "claude-rewind-ui-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, watch: null, hmr: false },
    plugins: [
      {
        name: "rewind-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/workspace/rewind-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id.endsWith("/servers/ServerServicesContext"))
            return fileURLToPath(new URL("./rewind-api.ts", import.meta.url));
        },
      },
    ],
  });
  const writes: PostBody<"/api/claude/session">[] = [];
  const errors: string[] = [];
  const releases: PostBody<"/api/claude/file-backups">[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.route("**/api/claude/file-backups", async (route) => {
      const body = fixturePostBody(route, "/api/claude/file-backups");
      if (body.release) releases.push(body);
      return route.fulfill({
        json: {
          count: body.release ? 99 : 100,
          bytes: 1000,
          maxCount: 100,
          maxBytes: 1073741824,
          released: body.release ? 1 : 0,
        },
      });
    });
    await page.route("**/api/claude/session", async (route) => {
      const body = fixturePostBody(route, "/api/claude/session");
      if (body.action === "state")
        return route.fulfill({
          json: {
            turns: [
              {
                id: "old",
                status: "completed",
                text: "Old message",
                canRestoreFiles: false,
                restoreFilesUnavailableReason:
                  "This message has no SDK file checkpoint.",
              },
              {
                id: "ready",
                status: "completed",
                text: "Edit the source file",
                canRestoreFiles: true,
                restoreFilesUnavailableReason: null,
              },
            ],
          },
        });
      writes.push(body);
      return route.fulfill(
        writes.length === 1
          ? {
              status: 400,
              json: {
                error:
                  "The before-restore backup limit is reached. Release completed backups before another restore.",
              },
            }
          : writes.length === 2
            ? {
                status: 409,
                json: {
                  error: "The restore reply was lost. Retry the same request.",
                },
              }
            : {
                json: {
                  threadId: "thread",
                  nativeId: "rewound",
                  filesRestored: true,
                },
              },
      );
    });
    await page.setViewportSize({ width: 320, height: 800 });
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
    const select = page.getByLabel("Message to restore before");
    const button = page.getByRole("button", {
      name: "Restore files and conversation",
    });
    await select.selectOption("old");
    await expect(button).toBeDisabled();
    await expect(
      page.getByText("This message has no SDK file checkpoint."),
    ).toBeVisible();
    await select.selectOption("ready");
    await expect(button).toBeEnabled();
    await page.screenshot({
      path: testInfo.outputPath("claude-rewind-320.png"),
      fullPage: true,
    });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await button.click();
    await expect(page.getByRole("alert")).toContainText("backup limit");
    await expect(
      page.getByText("Backup usage: 100 of 100 copies", { exact: false }),
    ).toBeVisible();
    await page.screenshot({
      path: testInfo.outputPath("claude-backup-quota-320.png"),
      fullPage: true,
    });
    await page
      .getByRole("button", { name: "Release completed backups" })
      .click();
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(releases).toHaveLength(1);
    await button.click();
    await expect(page.getByRole("alert")).toContainText(
      "The restore reply was lost.",
    );
    await expect(button).toBeEnabled();
    await button.click();
    await expect(select).toHaveValue("");
    expect(writes).toHaveLength(3);
    expect(writes[2]).toEqual(writes[0]);
    expect(writes[1]).toEqual(writes[0]);
    expect(writes[0]).toMatchObject({
      id: "worker",
      action: "rollback",
      turn_id: "ready",
      restore_files: true,
    });
    const firstWrite = required(writes[0]);
    if (firstWrite.action !== "rollback") throw new Error("Expected rollback");
    expect(firstWrite.request_id).toBeTruthy();
    await page.setViewportSize({ width: 1200, height: 800 });
    await select.selectOption("ready");
    await expect(button).toBeEnabled();
    await page.screenshot({
      path: testInfo.outputPath("claude-rewind-1200.png"),
      fullPage: true,
    });
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
