import { test, expect, spawnFixture, rustUiFixture } from "../playwright.mjs";
import { chooseSetupValue } from "../fixtures/setup-controls.mjs";
import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("new chat controls are inline, save settings and keep the compact composer after send", async ({
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
    const creation = page.getByRole("dialog", {
      name: "New chat",
      exact: true,
    });
    await expect(
      creation.getByLabel("Main agent model", { exact: true }),
    ).toBeVisible();
    await expect(creation.getByRole("listbox")).toHaveCount(1);
    await expect(
      creation.getByText("Main agent model", { exact: true }),
    ).toHaveCount(0);
    await expect(
      creation.getByText("Main agent reasoning", { exact: true }),
    ).toHaveCount(0);
    await expect(creation.locator(".model-description")).toHaveCount(0);
    await expect(page.getByRole("dialog")).toHaveCount(1);
    const screenshotDirectory = process.env.EXECUTION_COMPACT_SCREENSHOTS;
    const measurements = [];
    if (screenshotDirectory)
      await mkdir(screenshotDirectory, { recursive: true });
    for (const [width, height] of [
      [1440, 900],
      [390, 844],
    ]) {
      for (const scheme of ["light", "dark"]) {
        await page.setViewportSize({ width, height });
        await page.emulateMedia({ colorScheme: scheme });
        await expect(page.locator("html")).toHaveAttribute(
          "data-mantine-color-scheme",
          scheme,
        );
        const geometry = await creation
          .getByRole("listbox")
          .evaluate((node) => {
            const root = getComputedStyle(document.documentElement);
            return {
              picker: node.getBoundingClientRect().width,
              previous:
                parseFloat(root.getPropertyValue("--studio-form-max")) *
                parseFloat(root.fontSize),
              overflow: document.documentElement.scrollWidth > innerWidth,
            };
          });
        if (width === 1440)
          expect(geometry.picker).toBeCloseTo(geometry.previous * 0.65, 0);
        expect(geometry.overflow).toBe(false);
        measurements.push({ width, scheme, ...geometry });
        if (screenshotDirectory)
          await page.screenshot({
            animations: "disabled",
            path: join(screenshotDirectory, `new-chat-${width}-${scheme}.png`),
          });
      }
    }
    if (screenshotDirectory)
      await writeFile(
        join(screenshotDirectory, "new-chat-measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
    await page.setViewportSize({ width: 1440, height: 900 });
    await chooseSetupValue(
      creation.getByLabel("Main agent reasoning", { exact: true }),
      "high",
    );
    const roles = creation.getByRole("radiogroup", {
      name: "Agent role",
      exact: true,
    });
    await roles.getByText("Workers", { exact: true }).click();
    await expect(creation.getByRole("listbox")).toHaveCount(1);
    await expect(
      creation.getByLabel("Default subagent model", { exact: true }),
    ).toBeVisible();
    await chooseSetupValue(
      creation.getByLabel("Default subagent reasoning", { exact: true }),
      "medium",
    );
    await roles.getByText("Lead", { exact: true }).click();
    await expect(creation.getByRole("listbox")).toHaveCount(1);
    await expect(
      creation.getByLabel("Main agent reasoning", { exact: true }),
    ).toHaveAttribute("data-value", "high");
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
    const trigger = row.locator(".execution-summary-trigger");
    await expect(
      trigger.locator('[data-role="lead"] .execution-effort'),
    ).toContainText("High");
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
    const picker = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    await expect(
      picker.getByLabel("Main agent reasoning", { exact: true }),
    ).toHaveAttribute("data-value", "high");
    await picker.getByText("Workers", { exact: true }).click();
    await expect(
      page.getByLabel("Default subagent reasoning", { exact: true }),
    ).toHaveAttribute("data-value", "medium");
    await page.keyboard.press("Escape");
    await page.reload();
    await expect(
      trigger.locator('[data-role="lead"] .execution-effort'),
    ).toContainText("High");
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(trigger).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Attach files", exact: true }),
    ).toBeVisible();
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
