import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fixtureStateDirectory } from "../fixtures/rust-ui-fixture.mjs";
import { spawnFixture, test, expect, rustUiFixture } from "../playwright.mjs";

test("mutation projections propagate between pages sharing one browser context @sync", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const state = await mkdtemp(join(tmpdir(), "entity-tabs-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += String(chunk)));
  const context = await browser.newContext();
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const pages = [await context.newPage(), await context.newPage()];
    for (const page of pages) {
      await page.goto(`http://127.0.0.1:${port}`);
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .click();
      await expect(page.locator("#conversation-title")).toContainText(
        "Release lead",
      );
    }
    const id = await pages[0]
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .getAttribute("data-chat");
    assert.ok(id);
    const rename = async (page, current, name) => {
      const row = page.locator(`[data-sidebar-item="${id}"]`);
      await row.hover();
      await row
        .getByRole("button", { name: `Actions for ${current}`, exact: true })
        .click();
      await page.getByRole("menuitem", { name: "Rename", exact: true }).click();
      await row
        .getByRole("textbox", { name: "Chat name", exact: true })
        .fill(name);
      const response = page.waitForResponse(
        (response) =>
          response.request().method() === "POST" &&
          new URL(response.url()).pathname === "/api/rename",
      );
      await row.getByRole("button", { name: "Save name", exact: true }).click();
      assert.equal((await response).status(), 200);
    };
    // Mutate an existing durable chat so workspace provisioning is outside this fixture.
    await rename(pages[0], "Release lead", "First tab title");
    for (const page of pages) {
      await expect(page.locator(`[data-chat="${id}"]`)).toHaveText(
        "First tab title",
      );
    }
    await rename(pages[1], "First tab title", "Second tab title");
    for (const page of pages) {
      await expect(page.locator(`[data-chat="${id}"]`)).toHaveText(
        "Second tab title",
      );
    }
    // Reload the observer to check durable publication as well as live propagation.
    await pages[0].reload();
    await expect(pages[0].locator(`[data-chat="${id}"]`)).toHaveText(
      "Second tab title",
    );
  } finally {
    await context.close();
    if (fixture.exitCode === null && fixture.signalCode === null) {
      const exited = once(fixture, "exit", {
        signal: AbortSignal.timeout(10_000),
      });
      fixture.kill("SIGTERM");
      await exited;
    }
    await rm(state, { recursive: true, force: true });
    await rm(fixtureStateDirectory(state), { recursive: true, force: true });
  }
});
