import { test, expect } from "../playwright.mjs";
import { DEFAULT_SUBSCRIPTION_GRACE_MS } from "../../src/sync/subscriptions/contracts.ts";

test("chat list uses summaries and detail interest follows the visible chat", async ({
  page,
  readOnlyRust,
}) => {
  test.setTimeout(60_000);
  await page.setViewportSize({ width: 390, height: 844 });
  const additions = [];
  const removals = [];
  page.on("request", (request) => {
    if (
      new URL(request.url()).pathname !== "/api/sync/subscriptions" ||
      request.method() !== "POST"
    )
      return;
    const body = request.postDataJSON();
    additions.push(...body.add.map((item) => item.resource));
    removals.push(...body.remove);
  });
  await readOnlyRust.open(page);
  const row = page.locator("[data-chat]").first();
  await expect(row).toBeVisible();
  const agentId = await row.getAttribute("data-chat");
  expect(additions.filter((ref) => ref.kind === "agentDetail")).toEqual([]);
  const response = await page.request.get(
    `${readOnlyRust.origin}/api/sync/pull?scope=state:entities:v1&fresh=1&after=0&limit=500`,
  );
  expect(response.ok()).toBe(true);
  const initial = await response.json();
  expect(
    initial.documents.some((row) => row.collection === "agentSummary"),
  ).toBe(true);
  expect(
    initial.documents.some(
      (row) => row.collection === "agent" || row.collection === "agentDetail",
    ),
  ).toBe(false);
  await row.click();
  await expect
    .poll(() => additions)
    .toContainEqual({ kind: "agentDetail", agentId });
  await page.clock.install();
  await page
    .getByRole("button", { name: "Back to chats", exact: true })
    .click();
  await expect(row).toBeVisible();
  expect(removals).not.toContainEqual({ kind: "agentDetail", agentId });
  await page.clock.fastForward(DEFAULT_SUBSCRIPTION_GRACE_MS + 100);
  await expect
    .poll(() => removals)
    .toContainEqual({ kind: "agentDetail", agentId });
});
