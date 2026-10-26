import type { Route } from "@playwright/test";
import type { components } from "../../src/generated/api.ts";
import { requestBody } from "./helpers.ts";
import { required } from "./helpers.ts";
import type { ComplaintBody } from "./helpers.ts";
import type {} from "./complaint-ownership-ui.fixture.tsx";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import {
  test,
  createFixtureViteServer as createServer,
} from "../playwright.ts";

test("Complaint ownership", async ({ browser: testBrowser }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Current message recipient, history, and response delivery through the real component.
  const repo = testRepo;
  const server = await createServer({
    configFile: false,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "ownership-contract",
      },
    ],
  });
  await server.listen();
  let browserContext;
  const record = (
    id: string,
    author: string,
    recipient: "user" | "lead",
  ): components["schemas"]["ComplaintDetailResponse"] &
    components["schemas"]["ComplaintEntityDto"] => ({
    id,
    author,
    recipient,
    leadId: "lead",
    version: 1,
    text: `Message ${id}`,
    title: `Message ${id}`,
    status: "open",
    created: 1,
    updated: 1,
    readAt: null,
    responses: [],
    authorName: author === "lead" ? "Release lead" : "Worker",
    leadName: "Release lead",
    needsUserResponse: recipient === "user",
  });
  const user = record("owner", "lead", "user");
  const assigned = record("worker-request", "worker", "lead");
  const history = record("history", "lead", "user");
  history.responses = [
    {
      id: "historical-response",
      author: "lead",
      text: "The lead previously closed its own request.",
      status: "resolved",
      at: 1,
    },
  ];
  const records = [user, assigned, history];
  try {
    browserContext = await testBrowser.newContext({
      viewport: { width: 1200, height: 950 },
    });
    const page = await browserContext.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/tests/messages/complaint-ownership-ui.fixture.tsx";</script>',
      }),
    );
    await page.route("**/api/complaint?*", (route) =>
      route.fulfill({
        json: records.find(
          (record) =>
            record.id === new URL(route.request().url()).searchParams.get("id"),
        ),
      }),
    );
    let mode = "lost",
      completed = 0,
      pending: { route: Route | null } = { route: null };
    const pendingRoute = (): Route => {
      const route = pending.route;
      if (!route) throw new Error("Missing deferred reply");
      return route;
    };
    const attempts: ComplaintBody[] = [];
    await page.route("**/api/complaints", async (route) => {
      const payload = requestBody<ComplaintBody>(route);
      attempts.push(payload);
      assert.equal(route.request().headers()["x-canvas-token"], "fixture");
      if (mode === "deferred") {
        pending.route = route;
        return;
      }
      if (mode === "conflict") {
        user.version++;
        user.responses.push({
          id: "other-tab",
          author: "user",
          text: "Another window updated this message.",
          status: "in_progress",
          at: 2,
        });
        mode = "success";
        await route.fulfill({
          status: 409,
          json: { error: "Message version changed" },
        });
        return;
      }
      if (mode === "retry") {
        assert.deepEqual(payload, attempts[0]);
        mode = "conflict";
        await route.fulfill({ json: user });
        return;
      }
      assert.equal(payload.version, user.version);
      user.version++;
      completed++;
      user.responses.push({
        id: payload.id,
        author: "user",
        text: payload.text,
        status: required(payload.status),
        at: 3,
      });
      user.status = payload.status;
      user.needsUserResponse = false;
      if (mode === "lost") {
        mode = "retry";
        await route.abort("failed");
      } else await route.fulfill({ json: user });
    });
    await page.goto(required(server.resolvedUrls).local[0] + "check");
    await page.waitForFunction(() => !!window.ownershipSetRecords);
    await page.evaluate(
      (records) => window.ownershipSetRecords(records),
      records,
    );
    const card = (id: string) => page.locator(`[data-complaint="${id}"]`);
    const open = async (id: string) => {
      await card(id)
        .getByRole("button", { name: "Reply", exact: true })
        .click();
      await card(id)
        .getByRole("heading", { name: "Your reply", exact: true })
        .waitFor();
    };
    const close = (id: string) =>
      card(id)
        .getByRole("button", { name: "Close reply", exact: true })
        .click();
    assert.equal(await page.locator("[data-complaint]").count(), 2);
    const historyCard = page.locator('[data-complaint="history"]');
    assert.match(
      await historyCard.innerText(),
      /The lead previously closed its own request\./,
    );
    await historyCard
      .getByRole("button", { name: "Reply", exact: true })
      .waitFor();
    await open("history");
    assert.match(
      await historyCard.locator(".complaint-response").innerText(),
      /Release lead[\s\S]*The lead previously closed its own request\./,
    );
    assert.equal(
      required(history.responses[0]).status,
      "resolved",
      "Prior response metadata stays intact",
    );
    await historyCard
      .getByRole("heading", { name: "Your reply", exact: true })
      .waitFor();
    assert.equal(
      await historyCard
        .getByRole("button", { name: "Send reply", exact: true })
        .isDisabled(),
      true,
    );
    await close("history");
    await open("owner");
    assert.equal(user.readAt, null, "Opening does not mark the request read");
    await card("owner")
      .getByLabel("Reply", { exact: true })
      .fill("I will grant access.");
    await card("owner")
      .getByRole("button", { name: "Send reply", exact: true })
      .click();
    await card("owner")
      .getByRole("button", { name: "Retry response", exact: true })
      .waitFor();
    assert.equal(
      await card("owner").getByLabel("Reply", { exact: true }).isDisabled(),
      true,
    );
    await close("owner");
    await open("owner");
    await card("owner")
      .getByRole("button", { name: "Retry response", exact: true })
      .click();
    await card("owner")
      .getByRole("button", { name: "Send reply", exact: true })
      .waitFor();
    assert.equal(completed, 1);
    await card("owner")
      .getByLabel("Reply", { exact: true })
      .fill("Access is available.");
    await card("owner")
      .getByRole("button", { name: "Send reply", exact: true })
      .click();
    await card("owner")
      .getByRole("alert")
      .filter({ hasText: "This message changed" })
      .waitFor();
    await card("owner")
      .getByText("Another window updated this message.", { exact: true })
      .waitFor();
    assert.equal(attempts.length, 3);
    await card("owner")
      .getByRole("button", { name: "Send reply", exact: true })
      .click();
    await card("owner")
      .locator(".complaint-response")
      .filter({ hasText: "Access is available." })
      .waitFor();
    assert.equal(completed, 2);
    assert.notEqual(required(attempts[3]).id, required(attempts[2]).id);
    assert.equal(required(attempts[3]).status, "in_progress");
    await close("owner");
    await page.evaluate(() => window.ownershipSetTarget("lead"));
    await card("worker-request").waitFor();
    assert.equal(
      await card("worker-request")
        .getByRole("button", { name: "Reply", exact: true })
        .count(),
      0,
      "messages addressed to the lead do not expose the user's reply form",
    );
    await page.evaluate(() => window.ownershipSetTarget("user"));
    for (const status of [200, 409]) {
      await open("owner");
      await card("owner")
        .getByLabel("Reply", { exact: true })
        .fill(`Deferred ${status}`);
      mode = "deferred";
      pending.route = null;
      await card("owner")
        .getByRole("button", { name: "Send reply", exact: true })
        .click();
      for (let attempt = 0; !pending.route && attempt < 100; attempt++)
        await new Promise((resolve) => setTimeout(resolve, 10));
      assert.ok(pending.route);
      await close("owner");
      await open("history");
      await page.evaluate((forbidden) => {
        window.ownershipWrongMessage = false;
        window.ownershipObserver = new MutationObserver(() => {
          if (
            document
              .querySelector('[data-complaint="history"]')
              ?.textContent.includes(forbidden)
          )
            window.ownershipWrongMessage = true;
        });
        window.ownershipObserver.observe(document.body, {
          childList: true,
          subtree: true,
          characterData: true,
        });
      }, user.text);
      const settled = page.waitForResponse((response) =>
        response.url().endsWith("/api/complaints"),
      );
      const detail =
        status === 409
          ? page.waitForResponse((response) =>
              response.url().includes("/api/complaint?id=owner"),
            )
          : null;
      await pendingRoute().fulfill({
        status,
        json: status === 200 ? user : { error: "Message version changed" },
      });
      await settled;
      if (detail) await detail;
      await page.evaluate(async () => {
        await new Promise(requestAnimationFrame);
        await new Promise(requestAnimationFrame);
      });
      assert.equal(
        await page.evaluate(() => window.ownershipWrongMessage),
        false,
      );
      await historyCard.getByText(history.text, { exact: true }).waitFor();
      assert.equal(
        await historyCard.locator(".complaint-reply").count(),
        1,
        "Current historical records permit a user reply",
      );
      await page.evaluate(() => window.ownershipObserver.disconnect());
      await close("history");
    }
    await page.setViewportSize({ width: 390, height: 844 });
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: current recipients, preserved response history, pending user reply, exact retry, conflict, response permissions, late response, mobile",
    );
  } finally {
    await browserContext?.close();
    await server.close();
  }
});
