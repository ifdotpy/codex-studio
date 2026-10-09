import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Message info", async ({ browser: testBrowser }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(new URL("../../../", import.meta.url));
  // Production renderer and metadata HTTP reads, temp state, headless Chrome only.
  const repo = testRepo;
  const root = await mkdtemp(join(tmpdir(), "studio-message-info-"));
  const proc = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let browser, browserContext;
  let log = "";
  proc.stderr.on("data", (data) => {
    log += data;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const readState = async () => readTestState(origin);
    const initial = await readState();
    const actor = initial.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    browser = testBrowser;
    browserContext = await browser.newContext({
      viewport: { width: 1440, height: 1000 },
      timezoneId: "Europe/Warsaw",
      locale: "en-US",
      permissions: ["clipboard-read", "clipboard-write"],
    });
    const page = await browserContext.newPage();
    page.setDefaultTimeout(12000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const preferences = {
      theme: "light",
      typography: "original",
      contentLayout: "custom",
      sidebarFontSize: 14,
      mainFontSize: 14,
      fontFamily: "system",
      contentWidth: 60,
      sidebarShortcut: "Meta+b",
    };
    await page.addInitScript(
      (prefs) =>
        localStorage.setItem(
          "codex-studio-preferences-v1",
          JSON.stringify(prefs),
        ),
      preferences,
    );
    await page.route("**/api/session-cost?**", (route) =>
      route.fulfill({
        json: {
          rootId: actor.id,
          totalUSD: 0.1234,
          estimated: true,
          pricingState: "ready",
          unknownModels: [],
          breakdown: { providers: {}, models: {} },
        },
      }),
    );
    await page.goto(origin);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page.locator("#message").fill("Check the message metadata");
    await page.locator("#send").click();
    let agent;
    await assertEventually(async () => {
      agent = (await readState()).runtime.agents.find(
        (entry) => entry.id === actor.id,
      );
      return agent.status === "running" && agent.turnId;
    });
    const turnId = agent.turnId;
    const itemId = "answer-" + "long-message-identity-".repeat(8);
    const event = (method, params) =>
      proc.stdin.write(
        JSON.stringify({
          method,
          params: { threadId: agent.threadId, turnId, ...params },
        }) + "\n",
      );
    event("item/started", {
      item: { id: itemId, type: "agentMessage", text: "" },
    });
    event("item/agentMessage/delta", {
      itemId,
      delta: "Message metadata fixture.",
    });
    const message = page.locator(`[data-message="${agent.id}:${itemId}"]`);
    await message.waitFor();
    const target = message.getByRole("button", {
      name: "Message information",
      exact: true,
    });
    assert.equal(
      await target.count(),
      1,
      "streaming assistant has info control",
    );
    await target.focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", {
      name: "Message information",
      exact: true,
    });
    await dialog.waitFor();
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
    assert.equal(
      await target.evaluate((el) => el === document.activeElement),
      true,
      "Escape restores focus",
    );
    event("item/completed", {
      item: {
        id: itemId,
        type: "agentMessage",
        text: "Message metadata fixture.",
      },
    });
    event("thread/tokenUsage/updated", {
      responseId: "fixture-response",
      requestUsage: { outputTokens: 1200, reasoningOutputTokens: 350 },
      rawTokenUsageRecord: { response_id: "fixture-response" },
      tokenUsage: {
        last: { outputTokens: 1200, reasoningOutputTokens: 350 },
        total: { totalTokens: 2200 },
      },
    });
    event("turn/completed", {
      turn: { id: turnId, status: "completed" },
      durationMs: 2500,
    });
    let metadata;
    await assertEventually(async () => {
      metadata = await (
        await fetch(
          origin +
            "/api/analytics?" +
            new URLSearchParams({
              agent: agent.id,
              view: "message-info",
              item: itemId,
              turn: turnId,
            }),
        )
      ).json();
      return (
        metadata.tokens?.outputTokens === 1200 &&
        metadata.turnDurationMs === 2500 &&
        Number.isFinite(metadata.responseRate)
      );
    });
    await target.focus();
    await page.keyboard.press("Space");
    await dialog.getByText("Response output tokens", { exact: true }).waitFor();
    const value = async (label) =>
      dialog
        .locator(".message-info-row")
        .filter({ has: page.locator("dt", { hasText: label }) })
        .locator("dd")
        .innerText();
    assert.equal(await value("Run ID"), metadata.runId);
    assert.equal(await value("Attempt ID"), metadata.attemptId);
    assert.match(metadata.runId, /^run:/);
    assert.ok(metadata.attemptId);
    assert.equal(await value("Model"), agent.model);
    assert.equal(await value("Reasoning effort"), agent.effort);
    assert.equal(await value("Turn duration"), "2.5 s");
    assert.equal(await value("Response output tokens"), "1,200");
    assert.equal(await value("Response reasoning tokens"), "350");
    assert.match(await value("Response output rate"), /tok\/s$/);
    assert.equal(await value("Session estimate (USD)"), "$0.1234");
    assert.ok(await value("Account"));
    assert.ok(await value("Provider"));
    const transcript = await (
      await fetch(origin + "/api/transcript?id=" + agent.id)
    ).json();
    const source = transcript.items.find(
      (entry) => entry.id === agent.id + ":" + itemId,
    );
    const stamp = new Date((source.at ?? source.created) * 1000);
    const expectedParts = Object.fromEntries(
      new Intl.DateTimeFormat("en-US", {
        timeZone: "Europe/Warsaw",
        year: "numeric",
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
        hourCycle: "h23",
      })
        .formatToParts(stamp)
        .map(({ type, value }) => [type, value]),
    );
    const displayed = await value("Local date and time");
    const fields = displayed.match(
      /^([A-Za-z]{3}) (\d{1,2}), (\d{4}), (\d{2}):(\d{2}):(\d{2})$/,
    );
    assert.ok(fields, `unexpected timestamp display: ${displayed}`);
    const month = new Map(
      [
        "Jan",
        "Feb",
        "Mar",
        "Apr",
        "May",
        "Jun",
        "Jul",
        "Aug",
        "Sep",
        "Oct",
        "Nov",
        "Dec",
      ].map((name, index) => [name, index]),
    );
    const displayedWallTime = Date.UTC(
      Number(fields[3]),
      month.get(fields[1]),
      Number(fields[2]),
      Number(fields[4]),
      Number(fields[5]),
      Number(fields[6]),
    );
    const expectedWallTime = Date.UTC(
      Number(expectedParts.year),
      month.get(expectedParts.month),
      Number(expectedParts.day),
      Number(expectedParts.hour),
      Number(expectedParts.minute),
      Number(expectedParts.second),
    );
    assert.ok(
      Math.abs(displayedWallTime - expectedWallTime) <= 1000,
      `message timestamp differs from its Europe/Warsaw source time by more than one second: ${displayed}`,
    );
    assert.equal(
      await dialog.getByText(/tokens per second/i).count(),
      0,
      "no guessed rate",
    );
    assert.equal(
      await page
        .locator("article.user")
        .getByRole("button", { name: "Message information", exact: true })
        .count(),
      0,
    );
    await dialog
      .getByRole("button", { name: "Copy turn id", exact: true })
      .click();
    assert.equal(
      await page.evaluate(() => navigator.clipboard.readText()),
      turnId,
    );
    await dialog
      .getByRole("button", { name: "Copy item id", exact: true })
      .click();
    assert.equal(
      await page.evaluate(() => navigator.clipboard.readText()),
      itemId,
    );
    await dialog
      .getByRole("button", { name: "Close message information", exact: true })
      .focus();
    await page.keyboard.press("Shift+Tab");
    assert.equal(
      await dialog
        .getByRole("button", { name: "Copy item id", exact: true })
        .evaluate((el) => el === document.activeElement),
      true,
      "focus stays in popover",
    );
    await page.screenshot({ path: join(root, "desktop-original-60.png") });
    await page.keyboard.press("Escape");
    await message
      .getByRole("button", { name: "Copy message", exact: true })
      .click();
    assert.equal(
      await page.evaluate(() => navigator.clipboard.readText()),
      "Message metadata fixture.",
    );
    await message
      .getByRole("button", { name: "Quote message", exact: true })
      .click();
    assert.match(
      await page.locator("#message").inputValue(),
      /Message metadata fixture/,
    );
    for (const width of [60, 80, 100]) {
      await page.evaluate(
        (width) =>
          document.documentElement.style.setProperty(
            "--studio-content-width-ratio",
            String(width / 100),
          ),
        width,
      );
      await target.click();
      await dialog.waitFor();
      await checkBounds(dialog, 1440);
      await page.keyboard.press("Escape");
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await target.scrollIntoViewIfNeeded();
    assert.equal(
      await target.evaluate((el) => getComputedStyle(el).opacity),
      "1",
      "phone control remains visible",
    );
    const targetBox = await target.boundingBox();
    assert.ok(
      targetBox.width >= 44 && targetBox.height >= 44,
      "phone target is 44 px",
    );
    await target.click();
    await dialog.waitFor();
    await checkBounds(dialog, 390);
    await dialog.getByText("Response output tokens", { exact: true }).waitFor();
    assert.ok(
      await dialog.evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
      "long IDs wrap",
    );
    await page.screenshot({ path: join(root, "phone-390.png") });
    await dialog
      .getByRole("button", { name: "Close message information", exact: true })
      .click();
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        ok: true,
        screenshots: [
          join(root, "desktop-original-60.png"),
          join(root, "phone-390.png"),
        ],
      }),
    );
  } catch (error) {
    console.error(log.slice(-3000));
    throw error;
  } finally {
    await browserContext?.close();
  }
  async function assertEventually(read) {
    for (let i = 0; i < 100; i++) {
      if (await read()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error("Fixture state did not arrive");
  }
  async function checkBounds(locator, width) {
    const box = await locator.boundingBox();
    const height = await locator.evaluate(() => innerHeight);
    assert.ok(
      box.y >= 0 && box.y + box.height <= height + 1,
      "popover fits viewport height",
    );
    assert.ok(
      box.x >= 0 && box.x + box.width <= width + 1,
      "popover fits viewport",
    );
  }
});
