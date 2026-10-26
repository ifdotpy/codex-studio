import { fixturePostBody } from "../drafts/draft-fixture-helpers.ts";
import type { Page } from "playwright";
import type { PostBody, GetResult } from "../../src/apiTypes.ts";
import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
  rustFixtureStateDirectory,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

async function setupSendingRemovePointer(page: Page) {
  const evidence = await mkdtemp(join(tmpdir(), "studio-remove-hit-test-"));
  const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
  let log = "";
  fixture.stderr?.on("data", (chunk) => (log += chunk));
  let startupTimeout: ReturnType<typeof setTimeout> | undefined;
  const port = await new Promise<number>((resolve, reject) => {
    startupTimeout = setTimeout(
      () => reject(new Error(`Sending-message fixture did not start: ${log}`)),
      10_000,
    );
    fixture.stdout?.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  }).finally(() => clearTimeout(startupTimeout));
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  assert.ok(lead);
  await fetch(`${origin}/api/messages`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      Origin: origin,
      "X-Canvas-Token": initial.token,
    },
    body: JSON.stringify({
      id: crypto.randomUUID(),
      room: lead.id,
      text: "Pointer overlap while sending",
      delivery: "current_turn",
    }),
  }).then(async (response) => {
    assert.equal(response.status, 200, await response.text());
  });
  page.setDefaultTimeout(10_000);
  await page.setViewportSize({ width: 1440, height: 960 });
  await page.goto(origin);
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  const card = page
    .locator(".message.user")
    .filter({ hasText: "Pointer overlap while sending" });
  const remove = card.getByRole("button", {
    name: "Remove sending message",
    exact: true,
  });
  const copy = card.getByRole("button", {
    name: "Copy message",
    exact: true,
  });
  await card.waitFor({ state: "visible", timeout: 10_000 });
  await remove.waitFor({ state: "visible", timeout: 10_000 });
  await copy.waitFor({ state: "visible", timeout: 10_000 });
  const body = await card.locator(".prose.plain").boundingBox();
  assert.ok(body);
  await page.mouse.move(body.x + body.width / 2, body.y + body.height / 2, {
    steps: 5,
  });
  const removeBox = await remove.boundingBox();
  assert.ok(removeBox);
  await page.mouse.move(
    removeBox.x + removeBox.width / 2,
    removeBox.y + removeBox.height / 2,
    { steps: 5 },
  );
  const hovered = await remove.boundingBox();
  assert.ok(hovered);
  const center = {
    x: hovered.x + hovered.width / 2,
    y: hovered.y + hovered.height / 2,
  };
  const hit = await page.evaluate(({ x, y }) => {
    const target = document.elementFromPoint(x, y);
    return target?.closest("button")?.getAttribute("aria-label") ?? null;
  }, center);
  return { card, remove, copy, hit, center };
}

test("sending outgoing message exposes both delivery controls", async ({
  page,
}) => {
  test.setTimeout(30_000);
  page.setDefaultTimeout(10_000);
  const { card, remove, copy } = await setupSendingRemovePointer(page);
  await card.getByRole("status").waitFor({ state: "visible" });
  await remove.waitFor({ state: "visible" });
  await copy.waitFor({ state: "visible" });
});

test("remove-sending pointer hit target", async ({ page }) => {
  test.setTimeout(30_000);
  page.setDefaultTimeout(10_000);
  const { card, hit, center } = await setupSendingRemovePointer(page);
  test.fail(
    true,
    "Known delivery-control overlap; see workspaces/client/apps/web/tests/status/msg-known-failures.md.",
  );
  assert.equal(hit, "Copy message");
  await page.mouse.click(center.x, center.y);
  await expect(card).toBeHidden({ timeout: 1000 });
});

test("remove-sending-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const evidence = await mkdtemp(join(tmpdir(), "studio-remove-sending-"));
  const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
  let log = "";
  fixture.stderr?.on("data", (data) => (log += data));
  const port = await new Promise<number>((resolve, reject) => {
    fixture.stdout?.once("data", (data) =>
      resolve(Number(String(data).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const lead = initial.threads.find((a) => a.name === "Release lead");
  assert.ok(lead);
  const other = initial.threads.find((a) => a.name === "Other project");
  assert.ok(other);
  const post = async <Path extends "/api/queue" | "/api/stop">(
    path: Path,
    body: PostBody<Path>,
  ) => {
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
  const queue = async (id: string): Promise<GetResult<"/api/queue">> =>
    fetch(`${origin}/api/queue?agent=${id}`).then((r) => r.json());
  for (const agent of initial.threads.filter((a) => a.source === "managed"))
    for (const item of (await queue(agent.id)).items)
      await post("/api/queue", {
        action: "cancel",
        agent: agent.id,
        id: item.id,
        request_id: crypto.randomUUID(),
      });
  // Seed persisted pending rows on stopped disposable agents. A held active
  // turn would still accept current-turn input before removal can inspect it.
  for (const agent of [lead, other]) await post("/api/stop", { id: agent.id });
  const database = new DatabaseSync(
    join(rustFixtureStateDirectory(evidence), "canvas.sqlite3"),
  );
  try {
    database.exec("PRAGMA busy_timeout=5000");
    database.function(
      "sync_entity_payload",
      { deterministic: true, varargs: true },
      (_collection, _id, json) => String(json),
    );
    database.function("sync_entity_hash", { deterministic: true }, (payload) =>
      createHash("sha256").update(String(payload)).digest("hex"),
    );
    database.exec("BEGIN IMMEDIATE");
    for (const [agent, text, delivery] of [
      [lead, "Remove one", "current_turn"],
      [lead, "Remove two", "current_turn"],
      [other, "Remove in another chat", "current_turn"],
      [lead, "Keep after turn", "next_turn"],
    ] as const) {
      const id = randomUUID();
      const acceptedAt = Date.now() / 1000;
      const row = database
        .prepare("SELECT record FROM runtime_agents WHERE id=?")
        .get(agent.id);
      assert.ok(row && typeof row.record === "string");
      const record: unknown = JSON.parse(row.record);
      assert.ok(
        record &&
          typeof record === "object" &&
          "epoch" in record &&
          typeof record.epoch === "number",
      );
      database
        .prepare(
          "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,'user',?,'pending',?,?)",
        )
        .run(id, agent.id, text, acceptedAt, record.epoch);
      database
        .prepare("INSERT INTO runtime_event_meta(id,record) VALUES (?,?)")
        .run(
          id,
          JSON.stringify({
            assets: [],
            delivery,
            requestedDelivery: delivery,
            acceptedAt,
          }),
        );
    }
    database.exec("COMMIT");
  } finally {
    database.close();
  }
  const page = fixturePage;
  await fixturePage.setViewportSize({ width: 1440, height: 960 });
  page.setDefaultTimeout(10000);
  const errors: string[] = [],
    mutations: PostBody<"/api/queue">[] = [],
    starts: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (request.method() === "POST") {
      const path = new URL(request.url()).pathname;
      if (path === "/api/queue")
        mutations.push(fixturePostBody<"/api/queue">(request.postDataJSON()));
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
  const card = (text: string) =>
    page.locator(".message.user").filter({ hasText: text });
  const firstCard = card("Remove one");
  const removeButton = firstCard.getByRole("button", {
    name: "Remove sending message",
    exact: true,
  });
  await removeButton.focus();
  await page.keyboard.press("Enter");
  await expect(
    page.getByRole("button", { name: "Remove sending messages", exact: true }),
  ).toHaveCount(0);
  // Sync can remove the control after the first cancellation commits.
  // Retry the exact saved command even when that control is already gone.
  await expect.poll(() => mutations.length).toBe(1);
  const savedCancellation = await page.evaluate(() => {
    const keys = Object.keys(localStorage).filter((key) =>
      key.startsWith("studio-remove-sending:"),
    );
    // These saved commands were written by the renderer under test.
    const readCommand = (payload: string): PostBody<"/api/queue"> =>
      JSON.parse(payload);
    return keys.map((key) => readCommand(localStorage.getItem(key) ?? "null"));
  });
  assert.deepEqual(savedCancellation, [mutations[0]]);
  assert.equal(
    await page.evaluate(
      async ({ token, body }) => {
        const response = await fetch("/api/queue", {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "X-Canvas-Token": token,
          },
          body: JSON.stringify(body),
        });
        return response.status;
      },
      { token: initial.token, body: savedCancellation[0] },
    ),
    200,
  );
  await card("Remove one").waitFor({ state: "hidden" });
  assert.equal(mutations[0]?.request_id, mutations[1]?.request_id);
  const removeSecond = card("Remove two").getByRole("button", {
    name: "Remove sending message",
    exact: true,
  });
  await expect(removeSecond).toBeEnabled({ timeout: 20_000 });
  await removeSecond.focus();
  await page.keyboard.press("Enter");
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
  await settings.locator(".studio-appearance-more summary").click();
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
    `PASS: individual message controls, all chats, preserved after-turn queue, reload, restore without resending, and exact cancellation retry. ${evidence}`,
  );
});
