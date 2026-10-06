import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("remove-sending-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-remove-sending-"));
  const fixture = spawn(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "1" },
    },
  );
  let log = "";
  fixture.stderr.on("data", (data) => (log += data));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const lead = initial.threads.find((a) => a.name === "Release lead");
  const other = initial.threads.find((a) => a.name === "Other project");
  const post = async (path, body) => {
    const response = await fetch(origin + path, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        Origin: origin,
        "X-Canvas-Token": initial.token,
      },
      body: JSON.stringify(body),
    });
    assert.equal(response.status, 200, await response.clone().text());
    return response.json();
  };
  const queue = (id) =>
    fetch(`${origin}/api/queue?agent=${id}`).then((r) => r.json());
  for (const agent of initial.threads.filter((a) => a.source === "managed"))
    for (const item of (await queue(agent.id)).items)
      await post("/api/queue", {
        action: "cancel",
        agent: agent.id,
        id: item.id,
        request_id: crypto.randomUUID(),
      });
  for (const [room, text, delivery] of [
    [lead.id, "Remove one", "after_tool"],
    [lead.id, "Remove two", "after_tool"],
    [other.id, "Remove in another chat", "after_tool"],
    [lead.id, "Keep after turn", "after_turn"],
  ])
    await post("/api/messages", {
      id: crypto.randomUUID(),
      room,
      text,
      delivery,
    });
  const page = fixturePage;
  await fixturePage.setViewportSize({ width: 1440, height: 960 });
  page.setDefaultTimeout(10000);
  const errors = [],
    mutations = [],
    starts = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (request.method() === "POST") {
      const path = new URL(request.url()).pathname;
      if (path === "/api/queue") mutations.push(request.postDataJSON());
      if (path === "/api/messages" || path === "/api/stop") starts.push(path);
    }
  });
  let loseResponse = true;
  await page.addInitScript(
    ({ stateDir, chat }) => {
      const key = `studio-removed-messages:${JSON.stringify([stateDir, "agent", chat])}:restored`;
      if (localStorage.getItem(key) === null)
        localStorage.setItem(key, "invalid JSON");
    },
    { stateDir: initial.stateDir, chat: lead.id },
  );
  await page.route("**/api/queue", async (route) => {
    if (route.request().method() !== "POST" || !loseResponse)
      return route.continue();
    loseResponse = false;
    await route.fetch();
    await route.abort("failed");
  });
  await page.goto(origin);
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  const card = (text) =>
    page.locator(".message.user").filter({ hasText: text });
  await card("Remove one")
    .getByRole("button", { name: "Remove sending message", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Remove sending messages", exact: true })
    .waitFor();
  // The first cancellation committed, but its response was lost. Retry that
  // same removal without a new command identity, even after the queue moves on.
  await card("Remove one")
    .getByRole("button", { name: "Remove sending message", exact: true })
    .click();
  await card("Remove one").waitFor({ state: "hidden" });
  assert.equal(mutations[0].request_id, mutations[1].request_id);
  await page
    .getByRole("button", { name: "Remove sending messages", exact: true })
    .click();
  await card("Remove two").waitFor({ state: "hidden" });
  assert.deepEqual(
    (await queue(lead.id)).items.map((item) => item.text),
    ["Keep after turn"],
  );
  await page.reload();
  await page.locator("#message").waitFor();
  assert.equal(await card("Remove one").count(), 0);
  assert.equal(await card("Remove two").count(), 0);
  await page
    .getByRole("button", { name: "Restore removed messages", exact: true })
    .click();
  await card("Remove one").waitFor();
  assert.equal(
    await card("Remove one")
      .getByRole("button", { name: "Remove sending message", exact: true })
      .count(),
    0,
  );
  assert.deepEqual(
    (await queue(lead.id)).items.map((item) => item.text),
    ["Keep after turn"],
  );
  // A background desktop window may not receive animation frames.
  await page.evaluate(() => {
    window.requestAnimationFrame = () => 0;
  });
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  const settings = page.getByRole("dialog", {
    name: "Studio settings",
    exact: true,
  });
  await settings.getByRole("tab", { name: "Appearance", exact: true }).click();
  await settings
    .getByRole("button", {
      name: "Remove all sending messages",
      exact: true,
    })
    .click();
  await page
    .getByText(
      "Removed 1 sending messages from this device. Work already sent continues.",
      { exact: true },
    )
    .waitFor();
  assert.equal((await queue(other.id)).items.length, 0);
  await page.keyboard.press("Escape");
  await page.locator(".chat-row").filter({ hasText: "Other project" }).click();
  assert.equal(await card("Remove in another chat").count(), 0);
  await page
    .getByRole("button", { name: "Restore removed messages", exact: true })
    .click();
  await card("Remove in another chat").waitFor();
  assert.deepEqual(
    starts,
    [],
    "Removal and restore do not start or stop agents or resend messages.",
  );
  expect(errors).toEqual([]);
  console.log(
    `PASS: one message, whole chat, all chats, preserved after-turn queue, reload, restore without resending, and exact cancellation retry. ${evidence}`,
  );
});
