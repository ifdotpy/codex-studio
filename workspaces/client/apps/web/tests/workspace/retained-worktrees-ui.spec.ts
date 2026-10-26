import { required } from "../teams/helpers.ts";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("retained folders of live and deleted workers show at both widths", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "retained-worktrees-ui-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, watch: null, hmr: false },
    plugins: [
      {
        name: "retained-worktrees-fixture",
        enforce: "pre",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/workspace/retained-worktrees-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (/\/servers\/ServerServicesContext(?:\.tsx)?$/.test(id))
            return fileURLToPath(new URL("./retained-api.ts", import.meta.url));
        },
      },
    ],
  });
  const errors: string[] = [];
  let cleanupComplete = false;
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.route("**/api/retained-worktrees?*", (route) =>
      route.fulfill({
        json: {
          rootId: "lead",
          retainedWorktrees: [
            // A live worker shows on its own card, not twice.
            {
              id: "worker",
              name: "Live worker",
              cleanup: { phase: "not_sent", reason: null, path: "/work/live" },
            },
            ...(cleanupComplete
              ? [
                  {
                    id: "deleted-worker",
                    name: "Deleted worker",
                    cleanup: {
                      phase: "unknown",
                      reason: null,
                      path: "/Users/example/Projects/studio/.worktrees/codex-agents/deleted-worker",
                    },
                  },
                ]
              : []),
          ],
        },
      }),
    );
    await page.setViewportSize({ width: 320, height: 900 });
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
    await expect(
      page.getByText("Worktree retained", { exact: true }),
    ).toHaveCount(1);
    // Cleanup finishes after the deleted worker is absent from the live team.
    await expect(page.getByText("Deleted worker", { exact: true })).toHaveCount(
      0,
    );
    cleanupComplete = true;
    await page.evaluate(() => window.retainedWorktreesRefresh());
    for (const text of ["Deleted worker", "Cleanup outcome unconfirmed"])
      await expect(page.getByText(text, { exact: true })).toBeVisible();
    await expect(page.getByText("Live worker", { exact: true })).toHaveCount(0);
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 1100 });
      await page.screenshot({
        path: testInfo.outputPath(`retained-worktrees-${width}.png`),
        fullPage: true,
      });
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth,
        ),
      ).toBe(true);
    }
    await page
      .getByRole("button", { name: "Open folder", exact: true })
      .first()
      .click();
    expect(
      await page.evaluate(() => window.retainedWorktreesOpenedFolder),
    ).toBe(
      "/Users/example/Projects/studio/.worktrees/codex-agents/deleted-worker",
    );
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
