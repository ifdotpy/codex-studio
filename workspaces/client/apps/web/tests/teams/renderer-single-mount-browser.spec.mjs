import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, spawnFixture, test } from "../playwright.mjs";

test("another entry URL keeps one Studio tree after lazy modules load", async ({
  page,
}) => {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-single-mount-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      state,
    ],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "2" },
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    await page.setViewportSize({ width: 1440, height: 960 });
    await page.goto(`http://127.0.0.1:${port}`);
    await expect(page.locator("#message")).toBeVisible({ timeout: 10_000 });
    await expect(page.locator(".terminal-dock")).toHaveCount(1);
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await page.locator("#team-toggle").click();
    await expect(page.locator("#team")).toHaveCount(1);
    await page.evaluate(async () => {
      window.studioOriginalSidebar = document.getElementById("sidebar");
      const entry = document.querySelector('script[type="module"][src]');
      const url = new URL(entry.src);
      url.searchParams.set("duplicate-entry", "1");
      await import(url.href);
    });
    await expect
      .poll(() =>
        page.evaluate(
          () =>
            document.getElementById("sidebar") === window.studioOriginalSidebar,
        ),
      )
      .toBe(true);
    await expect(page.locator("#team")).toHaveCount(1);
    await expect(page.locator(".terminal-dock")).toHaveCount(1);
    await page.locator("#team-close").click();
    await expect(page.locator("#team")).toHaveCount(0);
    await page.locator("#team-toggle").click();
    await expect(page.locator("#team")).toHaveCount(1);
    await page.locator("#team .worker").first().click();
    await expect(page.locator("#team .worker-entry.selected")).toHaveCount(1);
    await expect(page.locator("#team")).toHaveCount(1);
    await expect(page.locator(".terminal-dock")).toHaveCount(1);
  } finally {
    fixture.kill();
  }
});
