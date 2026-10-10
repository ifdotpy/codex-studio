import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { join } from "node:path";
import { test } from "../playwright.mjs";

test("Complaint ownership", async ({ browser: testBrowser }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Current message recipient, history, and response delivery through the real component.
  const repo = testRepo;
  const require = createRequire(
    join(repo, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const harness = `
  import React from 'react';
  import {createRoot} from 'react-dom/client';
  import {MantineProvider} from '@mantine/core';
  import '@mantine/core/styles.css';
  import UserMessages from '/src/components/shell/messages/UserMessages.tsx';
  const root=createRoot(document.getElementById('root'));
  let records=[],target='user';
  const render=()=>root.render(React.createElement(MantineProvider,null,
   React.createElement(UserMessages,{data:{stateDir:'ownership-contract',token:'fixture',threads:[{id:'lead',name:'Release lead'},{id:'worker',name:'Worker'}],runtime:{complaints:records}},target,refresh:async()=>{},notify:()=>{}})));
  window.setRecords=(value)=>{records=value;render()};
  window.setTarget=(value)=>{target=value;render()};
  `;
  const server = await createServer({
    configFile: false,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "ownership-contract",
        resolveId(id) {
          if (id === "virtual:ownership") return "\0ownership";
        },
        load(id) {
          if (id === "\0ownership") return harness;
        },
      },
    ],
  });
  await server.listen();
  let browserContext;
  const record = (id, author, recipient) => ({
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
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__ownership";</script>',
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
      pending;
    const attempts = [];
    await page.route("**/api/complaints", async (route) => {
      const payload = route.request().postDataJSON();
      attempts.push(payload);
      assert.equal(route.request().headers()["x-canvas-token"], "fixture");
      if (mode === "deferred") {
        pending = route;
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
        status: payload.status,
        at: 3,
      });
      user.status = payload.status;
      user.needsUserResponse = false;
      if (mode === "lost") {
        mode = "retry";
        await route.abort("failed");
      } else await route.fulfill({ json: user });
    });
    await page.goto(server.resolvedUrls.local[0] + "check");
    await page.waitForFunction(() => !!window.setRecords);
    await page.evaluate((records) => window.setRecords(records), records);
    const card = (id) => page.locator(`[data-complaint="${id}"]`);
    const open = async (id) => {
      await card(id)
        .getByRole("button", { name: "Reply", exact: true })
        .click();
      await card(id)
        .getByRole("heading", { name: "Your reply", exact: true })
        .waitFor();
    };
    const close = (id) =>
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
      history.responses[0].status,
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
    assert.notEqual(attempts[3].id, attempts[2].id);
    assert.equal(attempts[3].status, "in_progress");
    await close("owner");
    await page.evaluate(() => window.setTarget("lead"));
    await card("worker-request").waitFor();
    assert.equal(
      await card("worker-request")
        .getByRole("button", { name: "Reply", exact: true })
        .count(),
      0,
      "messages addressed to the lead do not expose the user's reply form",
    );
    await page.evaluate(() => window.setTarget("user"));
    for (const status of [200, 409]) {
      await open("owner");
      await card("owner")
        .getByLabel("Reply", { exact: true })
        .fill(`Deferred ${status}`);
      mode = "deferred";
      pending = null;
      await card("owner")
        .getByRole("button", { name: "Send reply", exact: true })
        .click();
      for (let attempt = 0; !pending && attempt < 100; attempt++)
        await new Promise((resolve) => setTimeout(resolve, 10));
      assert.ok(pending);
      await close("owner");
      await open("history");
      await page.evaluate((forbidden) => {
        window.wrongMessage = false;
        window.observer = new MutationObserver(() => {
          if (
            document
              .querySelector('[data-complaint="history"]')
              ?.textContent.includes(forbidden)
          )
            window.wrongMessage = true;
        });
        window.observer.observe(document.body, {
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
      await pending.fulfill({
        status,
        json: status === 200 ? user : { error: "Message version changed" },
      });
      await settled;
      if (detail) await detail;
      await page.evaluate(async () => {
        await new Promise(requestAnimationFrame);
        await new Promise(requestAnimationFrame);
      });
      assert.equal(await page.evaluate(() => window.wrongMessage), false);
      await historyCard.getByText(history.text, { exact: true }).waitFor();
      assert.equal(
        await historyCard.locator(".complaint-reply").count(),
        1,
        "Current historical records permit a user reply",
      );
      await page.evaluate(() => window.observer.disconnect());
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
