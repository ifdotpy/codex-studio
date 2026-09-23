#!/usr/bin/env node
// Isolated server and hidden browser. No model calls or user data.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-unified-messages-"));
const fixture = spawn(
  "/opt/homebrew/bin/python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  {
    env: {
      ...process.env,
      MESSAGES_UI_FIXTURE: "1",
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(
        ["test-model", "gpt-6-astra", "gpt-6-luna", "gpt-5.6-luna"].map(
          (model) => ({
            model,
            defaultReasoningEffort: "medium",
            supportedReasoningEfforts: [
              "low",
              "medium",
              "high",
              "xhigh",
              "max",
              "ultra",
            ].map((reasoningEffort) => ({ reasoningEffort })),
            serviceTiers: [{ id: "priority" }],
          }),
        ),
      ),
    },
    stdio: ["ignore", "pipe", "pipe"],
  },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => (log += chunk));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const data = await (await fetch(origin + "/api/state")).json();
  const lead = data.threads.find((agent) => agent.name === "Release lead");
  const message = data.runtime.complaints.find(
    (item) => item.recipient === "user" && item.leadId === lead.id,
  );
  assert.ok(message);
  browser = await chromium.launch({
    headless: true,
    executablePath:
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
    locale: "en-US",
    timezoneId: "UTC",
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  assert.equal(
    await page
      .getByRole("menuitem", { name: /^(Your tasks|Agent tasks)$/ })
      .count(),
    0,
  );
  await page.keyboard.press("Escape");
  assert.ok(
    await page.locator(".message > .message-date").count(),
    "Main chat messages have dates",
  );
  await page.locator(".message > .message-date[datetime]").first().waitFor();
  assert.equal(
    await page.locator(".message > .message-date:not([datetime])").count(),
    0,
    "Stored main chat dates use message.at",
  );
  await page.locator("#messages-toggle").click();
  const chats = page.locator(".messages-chat-list");
  const card = chats.locator(`[data-complaint="${message.id}"]`);
  await card
    .getByText("Please confirm the release scope.", { exact: true })
    .last()
    .waitFor();
  const date = card.locator(":scope > time");
  assert.equal(
    await date.getAttribute("datetime"),
    new Date(message.created * 1000).toISOString(),
  );
  assert.match(await date.innerText(), /\d{4}/);
  assert.match(await date.innerText(), /\d{2}:\d{2}/);
  assert.equal(
    await card.locator("select").count(),
    0,
    "No message status workflow",
  );
  const questionDate = chats.locator(
    '[data-feed-item="request:async-question"] time',
  );
  assert.equal(
    await questionDate.getAttribute("datetime"),
    new Date(1790074800 * 1000).toISOString(),
  );
  assert.match(await questionDate.innerText(), /Sep 22, 2026/);
  assert.equal(
    await chats.locator(".unified-action-items").count(),
    0,
    "Agent failures do not append status cards to Messages",
  );
  const payloads = [];
  let lose = true;
  await page.route("**/api/complaints", async (route) => {
    payloads.push(route.request().postDataJSON());
    if (lose) {
      lose = false;
      await route.fetch();
      await route.abort("failed");
    } else await route.continue();
  });
  await card.getByRole("button", { name: "Reply", exact: true }).click();
  await card
    .getByRole("textbox", { name: "Reply", exact: true })
    .fill("Please continue with the confirmed scope.");
  await card.getByRole("button", { name: "Send reply", exact: true }).click();
  await card
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  await card
    .getByRole("button", { name: "Retry response", exact: true })
    .click();
  await card
    .locator(".complaint-response")
    .getByText("Please continue with the confirmed scope.", { exact: true })
    .waitFor();
  assert.equal(payloads.length, 2);
  assert.deepEqual(
    payloads[0],
    payloads[1],
    "Lost response retry preserves exact payload",
  );
  assert.equal(await card.locator(".complaint-response").count(), 1);
  assert.ok(
    await card.locator(".complaint-response time").getAttribute("datetime"),
  );
  const room = data.runtime.rooms.find(
    (item) => item.kind === "private" && item.members.includes(lead.id),
  );
  await chats.getByRole("textbox", { name: "Search chats" }).fill("Worker 39");
  await chats.locator(`[data-room="${room.id}"]`).click();
  await chats.locator(".team-message").first().waitFor();
  assert.match(
    await chats.locator(`[data-room="${room.id}"] time`).innerText(),
    /\d{4}/,
  );
  assert.ok(
    await chats.locator(".team-message time").first().getAttribute("datetime"),
  );
  await chats.getByRole("textbox", { name: "Search chats" }).fill("");
  await chats.locator('[data-room="you"]').click();
  await card
    .locator(".complaint-response")
    .getByText("Please continue with the confirmed scope.", { exact: true })
    .waitFor();
  await page.screenshot({ path: join(evidence, "desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(
    await chats.evaluate((node) => node.scrollWidth <= node.clientWidth),
  );
  await page.screenshot({ path: join(evidence, "mobile.png") });
  await chats.getByRole("button", { name: "Back to chats" }).click();
  assert.ok(await chats.locator('[data-room="you"] time').isVisible());
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
