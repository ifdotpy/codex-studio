import type { GetResult } from "../../src/apiTypes.ts";
import { required } from "../teams/helpers.ts";
import { createServer } from "vite";
import type {} from "./claude-repair-ui.fixture.tsx";
import { apiSchemaHeaders, test, expect } from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// Real chat components with a read-only HTTP fixture. No provider query or desktop input.
test("Claude unconfirmed turn and journal repair at 320 and 1200 pixels", async ({
  page,
}, testInfo) => {
  test.setTimeout(120000);
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const scratch = await mkdtemp(join(tmpdir(), "claude-repair-ui-"));

  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(scratch, "vite"),
    plugins: [
      {
        name: "claude-repair-fixture",
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  const writes: string[] = [],
    errors: string[] = [];
  let reportStatus: GetResult<"/api/claude/repair-report">["status"] =
    "needs_repair";
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module">import "/tests/errors/claude-repair-ui.fixture.tsx";</script>',
    }),
  );
  await page.route("**/api/**", (route) => {
    const request = route.request();
    if (request.method() !== "GET")
      writes.push(request.method() + " " + request.url());
    const path = new URL(request.url()).pathname;
    return route.fulfill({
      headers: apiSchemaHeaders(),
      json:
        path === "/api/claude/repair-report"
          ? ({
              status: reportStatus,
              skippedRows: 3,
              invalidSequence: reportStatus === "needs_repair",
              scopeConfirmed: true,
              nativeHistoryConfirmed: reportStatus === "repaired",
            } satisfies GetResult<"/api/claude/repair-report">)
          : { items: [], turns: [], requests: [] },
    });
  });
  await server.listen();
  try {
    for (const width of [320, 1200]) {
      for (const status of ["needs_repair", "repaired"] as const) {
        reportStatus = status;
        await page.setViewportSize({ width, height: 1000 });
        await page.goto(required(server.resolvedUrls?.local[0]) + "check");
        await expect(
          page.getByText("Outcome unconfirmed", { exact: true }),
        ).toBeVisible();
        await expect(
          page.getByText(
            status === "repaired" ? "Journal repaired" : "Journal needs repair",
            { exact: true },
          ),
        ).toBeVisible();
        await expect(
          page.getByText("Skipped rows: 3.", { exact: true }),
        ).toBeVisible();
        await page.getByText("Repair report", { exact: true }).click();
        await expect(
          page.getByText("Missing evidence", { exact: true }),
        ).toBeVisible();
        await expect(
          page.getByText(
            status === "repaired"
              ? "No missing evidence was reported."
              : "A complete journal sequence.",
            { exact: true },
          ),
        ).toBeVisible();
        await expect(page.locator('[data-outcome="uncertain"]')).toBeVisible();
        await expect(
          page.getByRole("button", {
            name: /retry|replay|repair|another answer/i,
          }),
        ).toHaveCount(0);
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
        const screenshot = testInfo.outputPath(
          `claude-repair-${status}-${width}.png`,
        );
        await page.screenshot({ path: screenshot, fullPage: true });
        await testInfo.attach(`claude-repair-${status}-${width}`, {
          path: screenshot,
          contentType: "image/png",
        });
      }
    }
    expect(writes).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(scratch, { recursive: true, force: true });
  }
});
