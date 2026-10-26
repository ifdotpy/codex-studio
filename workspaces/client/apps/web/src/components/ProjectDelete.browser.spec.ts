import { fixtureOrigin } from "../../tests/misc-fixture.ts";
import type {} from "./ProjectDelete.browser.fixture.tsx";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.ts";

for (const width of [320, 1200]) {
  test(`project deletion menu and confirmation at ${width}px`, async ({
    browser,
  }) => {
    const server = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "project-delete-fixture",
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              if (!request.url?.startsWith("/project-delete-fixture"))
                return next();
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/src/components/ProjectDelete.browser.fixture.tsx"></script>',
              );
            });
          },
        },
      ],
    });
    const page = await browser.newPage({
      viewport: { width, height: 900 },
      hasTouch: width === 320,
    });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await server.listen();
      const url = `${fixtureOrigin(server)}/project-delete-fixture`;
      for (const variant of ["", "?archive", "?empty", "?logical"]) {
        await page.goto(url + variant);
        if (variant === "?archive") {
          await page
            .getByRole("button", { name: "Project list options" })
            .click();
          await page
            .getByRole("menuitem", { name: "Show archived chats" })
            .click();
        }
        const options = page.getByRole("button", {
          name: "Options for project Example",
        });
        await options.focus();
        await page.keyboard.press("Enter");
        const item = page.getByRole("menuitem", { name: "Delete project…" });
        await expect(item).toBeVisible();
        if (width === 320)
          expect((await item.boundingBox())?.height).toBeGreaterThanOrEqual(44);
        await item.focus();
        await page.keyboard.press("Enter");
        const dialog = page.getByRole("dialog", {
          name: "Delete project",
          exact: true,
        });
        await expect(dialog).toContainText("archived chats");
        await expect(dialog).toContainText("Running agents will be stopped");
        await expect(dialog).toContainText("Git worktrees are kept");
        await dialog
          .getByRole("button", { name: "Cancel", exact: true })
          .click();
        expect(
          await page.evaluate(() => window.projectDeleteFixture.posts),
        ).toEqual([]);
        await options.focus();
        await page.keyboard.press("Enter");
        await page.getByRole("menuitem", { name: "Delete project…" }).click();
        await page.evaluate(() => {
          window.projectDeleteFixture.fail = true;
        });
        // Two clicks in one task exercise the synchronous intention lock before React commits.
        await dialog
          .getByRole("button", { name: "Delete project", exact: true })
          .evaluate((button) => {
            if (!(button instanceof HTMLButtonElement))
              throw new Error("Missing deletion button");
            button.click();
            button.click();
          });
        const unknown = page.getByRole("dialog", {
          name: "Delete result unconfirmed",
        });
        await expect(unknown).toBeVisible();
        expect(
          await page.evaluate(() => window.projectDeleteFixture.posts.length),
        ).toBe(1);
        if (variant !== "?empty")
          expect(
            await page.evaluate(() => window.projectDeleteFixture.suspended),
          ).toContain("lead");
        expect(
          await page.evaluate(() => window.projectDeleteFixture.finished),
        ).toEqual([]);
        await page.evaluate(() => window.projectDeleteFixture.retire());
        await expect(options).toHaveCount(0);
        await expect(unknown).toBeVisible();
        await page.evaluate(() => {
          window.projectDeleteFixture.fail = false;
        });
        await unknown
          .getByRole("button", { name: "Recover saved request" })
          .click();
        await expect(unknown).toHaveCount(0);
        const posts = await page.evaluate(
          () => window.projectDeleteFixture.posts,
        );
        expect(posts).toHaveLength(2);
        expect(posts[1]).toEqual(posts[0]);
        if (variant === "?logical")
          expect(
            await page.evaluate(() => window.projectDeleteFixture.owner),
          ).toBe("owner");
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
      }
      expect(errors).toEqual([]);
    } finally {
      await page.close();
      await server.close();
    }
  });
}
