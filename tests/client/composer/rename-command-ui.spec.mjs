import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("/rename updates the sidebar without sending a message", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-rename-ui-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        RENAME_UI_FIXTURE: "1",
        TOKEN_RATE_WORKER_COUNT: "1",
      },
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.threads.find((agent) => agent.name === "Release lead");
    assert.ok(lead);
    const page = await context.newPage();
    const messages = [];
    await page.route("**/api/messages", (route) => {
      messages.push(route.request().postDataJSON());
      return route.abort();
    });
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const input = page.locator("#message");
    await input.fill("/re");
    await page
      .getByRole("option", { name: /rename.*Name this chat/ })
      .waitFor();
    await input.press("Escape");
    assert.equal(
      await page
        .getByRole("option", { name: /rename.*Name this chat/ })
        .count(),
      0,
    );
    await input.fill("/ren");
    await page.getByRole("option", { name: /rename.*Name this chat/ }).click();
    assert.equal(await input.inputValue(), "/rename ");
    await page.getByRole("button", { name: /Send/ }).last().click();
    await page.waitForFunction(
      (id) =>
        document
          .querySelector(`[data-chat="${id}"]`)
          ?.textContent.includes("Release review and blockers"),
      lead.id,
    );
    assert.equal(messages.length, 0);
    await page.screenshot({
      path: join(evidence, "rename-1440.png"),
      fullPage: true,
    });
    await input.fill("/rename Exact release title");
    await page.getByRole("button", { name: "Send message" }).click();
    await page.waitForFunction(() =>
      document
        .querySelector("#conversation-title")
        ?.textContent.includes("Exact release title"),
    );
    await input.fill("/rename");
    await input.press("Enter");
    await page.waitForFunction(() =>
      document
        .querySelector("#conversation-title")
        ?.textContent.includes("Release review and blockers"),
    );
    assert.equal(messages.length, 0);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({
      path: join(evidence, "rename-390.png"),
      fullPage: true,
    });
    console.log(
      `Screenshots: ${evidence}/rename-1440.png ${evidence}/rename-390.png`,
    );
  } finally {
    await context.close();
    fixture.kill();
  }
});
