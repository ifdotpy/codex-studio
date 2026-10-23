import { test, expect, spawnFixture, rustUiFixture } from "../playwright.mjs";
import { chooseSetupValue } from "../fixtures/setup-controls.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("new chat controls open each role, save settings and stay in the composer after send", async ({
  page,
}) => {
  test.setTimeout(120000);
  const evidence = await mkdtemp(join(tmpdir(), "studio-new-chat-settings-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, evidence], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      FIXTURE_WORKER_COUNT: "1",
    },
  });
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
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(`http://127.0.0.1:${port}`);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .first()
      .click();
    await page
      .locator(".sidebar-nav")
      .getByRole("button", { name: "New chat", exact: true })
      .click();
    const startChat = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        response.url().endsWith("/api/leads"),
    );
    await page
      .getByRole("dialog", { name: "New chat", exact: true })
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await startChat;
    const row = page.locator("#composer .composer-context-row");
    await expect(row).toBeVisible();
    await expect(
      page
        .getByRole("navigation", { name: "Recent project chats" })
        .getByRole("button", { name: /Release lead/ }),
    ).toBeVisible();
    await expect(row.locator(".execution-menu")).toHaveCount(1);
    await expect(page.locator("#composer .execution-menu")).toHaveCount(1);
    await expect(page.locator(".empty-chat p")).toHaveCount(0);
    const trigger = row.getByRole("button", { name: /^Execution settings:/ });
    const main = trigger.locator('[data-role="lead"]');
    const workers = trigger.locator('[data-role="worker"]');
    await expect(main.locator(".execution-model")).not.toBeEmpty();
    await expect(main.locator(".execution-effort")).not.toBeEmpty();
    await expect(workers.locator(".execution-model")).toHaveText("Luna 6");
    await expect(workers.locator(".execution-effort")).toHaveText("High");
    await expect(trigger).toHaveAttribute(
      "aria-label",
      /Review same as caller/,
    );
    const composerBounds = await page.locator("#composer").boundingBox();
    const rowBounds = await row.boundingBox();
    const startBounds = await page.locator(".conversation-start").boundingBox();
    expect(Math.abs(startBounds.x - composerBounds.x)).toBeLessThanOrEqual(1);
    expect(
      Math.abs(startBounds.width - composerBounds.width),
    ).toBeLessThanOrEqual(1);
    expect(rowBounds.y).toBeGreaterThanOrEqual(composerBounds.y);
    expect(rowBounds.y + rowBounds.height).toBeLessThanOrEqual(
      composerBounds.y + composerBounds.height,
    );
    await trigger.click();
    await expect(
      page.getByRole("radio", { name: /Lead/, exact: false }),
    ).toBeChecked();
    const mainSave = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/conversation") &&
        response.request().postDataJSON()?.effort === "high",
    );
    await chooseSetupValue(
      page.getByLabel("Main agent reasoning", { exact: true }),
      "high",
    );
    expect((await mainSave).ok()).toBe(true);
    await page.keyboard.press("Escape");
    await expect(
      page.getByRole("dialog", { name: "Chat settings", exact: true }),
    ).toHaveCount(0);
    await expect(main.locator(".execution-effort")).toHaveText("High");
    await trigger.click();
    await page
      .getByRole("radiogroup", { name: "Agent role", exact: true })
      .getByText("Workers", { exact: true })
      .click();
    await expect(
      page.getByRole("radio", { name: /Workers/, exact: false }),
    ).toBeChecked();
    const workerSave = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/conversation") &&
        response.request().postDataJSON()?.worker_defaults?.effort === "medium",
    );
    await chooseSetupValue(
      page.getByLabel("Default subagent reasoning", { exact: true }),
      "medium",
    );
    expect((await workerSave).ok()).toBe(true);
    await page.keyboard.press("Escape");
    await expect(
      page.getByRole("dialog", { name: "Chat settings", exact: true }),
    ).toHaveCount(0);
    await expect(workers.locator(".execution-effort")).toHaveText("Medium");
    await page.reload();
    await expect(main.locator(".execution-effort")).toHaveText("High");
    await expect(workers.locator(".execution-effort")).toHaveText("Medium");
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(trigger).toBeVisible();
    await expect(trigger.locator(".execution-role-icon")).toHaveCount(3);
    expect(
      await trigger.evaluate((node) => node.scrollWidth <= node.clientWidth),
    ).toBe(true);
    await page.locator("#message").fill("Fixture first message");
    const send = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/messages") &&
        response.request().method() === "POST",
    );
    await page.locator("#send").click();
    expect((await send).ok()).toBe(true);
    await expect(page.locator(".conversation-start")).toHaveCount(0);
    await expect(row).toBeVisible();
    await expect(page.locator("#composer .execution-menu")).toHaveCount(1);
    const footer = page.locator(".conversation-composer-footer");
    await expect(trigger).toBeVisible();
    await expect(footer.locator(".execution-menu")).toHaveCount(0);
  } finally {
    fixture.kill("SIGTERM");
  }
});
