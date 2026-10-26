import type { PostBody, PostResult } from "../../src/apiTypes.ts";
import { fixturePostBody } from "./fixture-server.ts";
import { fixtureUrl, usageFixtureServer } from "./fixture-server.ts";
import assert from "node:assert/strict";
import { test } from "../playwright.ts";

test("Usage resume notice", async ({ context: testContext }) => {
  test.setTimeout(180_000);
  // Render the production limit notice and verify its schedule and opt-out action.

  const dueAt = Math.floor(Date.now() / 1000) + 3600;

  const server = await usageFixtureServer("usage-resume-notice-ui");
  try {
    await server.listen();
    const page = await testContext.newPage();
    const writes: (PostBody<"/api/usage-resume"> | string)[] = [];
    page.on("pageerror", (error) => writes.push(`pageerror:${error.message}`));
    await page.route("**/api/usage-resume", async (route) => {
      const body = fixturePostBody<"/api/usage-resume">(
        route.request().postDataJSON(),
      );
      writes.push(body);
      await route.fulfill({
        json: {
          id: body.resume_id ?? "",
          accountKey: "work",
          cause: body.id === "chat-auth" ? "auth" : "usage_limit",
          epoch: 0,
          failedAt: 0,
          threadId: "thread-1",
          turnId: "",
          status: "cancelled",
          dueAt: null,
          reason: "Automatic resume is off for this chat.",
          updatedAt: 2,
        } satisfies PostResult<"/api/usage-resume">,
      });
    });
    await page.goto(fixtureUrl(server) + `?dueAt=${dueAt}`);
    const notices = page.locator(".account-limit-recovery.inline");
    await notices.first().waitFor();
    const notice = notices.filter({ hasText: "Usage limit reached" }).first();
    const authNotice = notices.nth(1);
    assert.equal(
      await notices
        .filter({ hasText: "Waiting for the account sign-in" })
        .count(),
      1,
    );
    assert.match(
      (await notice.textContent()) ?? "",
      /Automatic resume planned for/,
    );
    assert.match((await authNotice.textContent()) ?? "", /Next account check/);
    await authNotice
      .getByRole("button", { name: "Turn off automatic resume" })
      .click();
    await authNotice
      .getByRole("button", { name: "Turn on automatic resume" })
      .waitFor();
    assert.match(
      (await authNotice.textContent()) ?? "",
      /Automatic resume is off/,
    );
    assert.ok(
      (await notice
        .locator(`time[datetime="${new Date(dueAt * 1000).toISOString()}"]`)
        .count()) >= 1,
    );
    await notice
      .getByRole("button", { name: "Turn off automatic resume" })
      .click();
    await notice
      .getByRole("button", { name: "Turn on automatic resume" })
      .waitFor();
    assert.deepEqual(writes, [
      { id: "chat-auth", resume_id: "auth-resume-1", enabled: false },
      { id: "chat-1", resume_id: "resume-1", enabled: false },
    ]);
    console.log("Usage resume notice: planned time and opt-out toggle passed.");
  } finally {
    await server.close();
  }
});
