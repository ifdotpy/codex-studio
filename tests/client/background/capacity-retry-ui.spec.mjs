import {
  readTestState,
  readLegacySnapshotForS2Assertions,
  spawnFixture as spawn,
  test,
} from "../playwright.mjs";
// Real runtime, SQLite and two browser pages. The native server is a fixture.
import assert from "node:assert/strict";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("capacity retry ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const rootDir = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "studio-capacity-ui-"));
  const fixture = spawn(
    "python3",
    ["-B", join(rootDir, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        CAPACITY_UI_FIXTURE: "1",
      },
    },
  );
  let log = "",
    browser;
  fixture.stderr.on("data", (data) => (log += data));
  const until = async (fn, name, timeout = 16000) => {
    const end = Date.now() + timeout;
    while (Date.now() < end) {
      if (await fn()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error(`${name}: ${log}`);
  };
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (data) =>
        resolve(Number(String(data).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = () => readTestState(origin);
    const snapshot = await state();
    const id = snapshot.runtime.agents.find(
      (a) => a.name === "Other project",
    ).id;
    const agent = async () =>
      (await state()).runtime.agents.find((a) => a.id === id);
    const legacyAgent = async () =>
      (await readLegacySnapshotForS2Assertions(origin)).runtime.agents.find(
        (a) => a.id === id,
      );
    let threadId;
    const starts = async () =>
      (await readFile(join(root, "capacity-starts.jsonl"), "utf8"))
        .trim()
        .split("\n")
        .filter(Boolean)
        .map(JSON.parse)
        .filter((entry) => !threadId || entry.threadId === threadId);
    browser = _browser;
    const context = await browser.newContext({
      viewport: { width: 1200, height: 900 },
    });
    const page = await context.newPage(),
      other = await context.newPage();
    const errors = [];
    for (const p of [page, other]) {
      p.on("pageerror", (error) => errors.push(error.message));
      await p.goto(origin);
      await p.locator(`[data-chat="${id}"]`).click();
    }
    await page
      .locator("#message")
      .fill("Keep the original user message exactly once.");
    await page.locator("#send").click();
    await until(async () => (await agent()).turnId, "original turn");
    const first = await agent();
    threadId = first.threadId;
    const notifyFailure = (a) =>
      fixture.stdin.write(
        JSON.stringify({
          method: "turn/completed",
          params: {
            threadId: a.threadId,
            turn: {
              id: a.turnId,
              status: "failed",
              error: {
                message: "Model is at capacity",
                codexErrorInfo: "serverOverloaded",
              },
            },
          },
        }) + "\n",
      );
    notifyFailure(first);
    let projectedAgent;
    await until(
      async () => {
        const entityPull = await fetch(
          `${origin}/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=500`,
        );
        assert.equal(entityPull.status, 200);
        const entityRows = (await entityPull.json()).documents;
        projectedAgent = entityRows
          .map((row) => JSON.parse(row.payload))
          .find(
            (payload) => payload.collection === "agent" && payload.id === id,
          );
        return projectedAgent?.value.capacityRetry?.status === "scheduled";
      },
      "Runtime.put did not publish the scheduled retry in the agent entity",
      8000,
    );
    assert.equal(projectedAgent.value.capacityRetry.status, "scheduled");
    await page
      .locator('.capacity-retry[data-retry-status="scheduled"]')
      .waitFor();
    await until(
      async () => (await agent()).lastCompletedTurn === first.turnId,
      "failure event was not recorded",
      8000,
    );
    await until(
      async () => (await agent()).capacityRetry?.status === "scheduled",
      "scheduled retry missing",
      8000,
    );
    const updateDialog = page.locator('[aria-label="Studio update required"]');
    assert.equal(
      await updateDialog.isVisible(),
      false,
      "a matching schema must not open the update dialog",
    );
    await page.locator("#message").fill("Keep this draft while retrying.");
    await page.setViewportSize({ width: 320, height: 740 });
    await page
      .getByRole("button", { name: "Cancel automatic retry", exact: true })
      .click();
    await page
      .getByText("Automatic retry cancelled.", { exact: true })
      .waitFor();
    await other
      .getByText("Automatic retry cancelled.", { exact: true })
      .waitFor();
    await page.reload();
    await page
      .getByText("Automatic retry cancelled.", { exact: true })
      .waitFor();
    assert.equal(await page.evaluate(() => document.body.scrollWidth), 320);
    await page.screenshot({ path: join(root, "capacity-cancel-mobile.png") });
    await new Promise((resolve) => setTimeout(resolve, 10500));
    assert.equal(
      (await starts()).length,
      1,
      "cancel survives reload and the original deadline",
    );
    const retryId = (await legacyAgent()).capacityRetry.id;
    let lost = false;
    await page.route("**/api/capacity-retry", async (route) => {
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      lost = true;
      await route.abort("failed");
    });
    await page.getByRole("button", { name: "Retry now", exact: true }).click();
    await until(
      async () =>
        (await agent()).turnId && (await agent()).turnId !== first.turnId,
      "manual empty turn",
    );
    assert.ok(lost);
    const duplicate = await fetch(`${origin}/api/capacity-retry`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": (await state()).token,
      },
      body: JSON.stringify({ id, retry_id: retryId, action: "retry" }),
    });
    assert.equal(duplicate.status, 200);
    assert.equal(
      (await starts()).length,
      2,
      "lost response and duplicate action produce one native turn",
    );
    assert.deepEqual((await starts())[1].input, []);
    assert.equal(
      await page.locator("#message").inputValue(),
      "Keep this draft while retrying.",
    );
    await page.unroute("**/api/capacity-retry");
    const second = await agent();
    notifyFailure(second);
    await until(
      async () => (await legacyAgent()).capacityRetry.status === "scheduled",
      "next retry timer",
    );
    assert.equal(
      Math.round(
        (await legacyAgent()).capacityRetry.dueAt -
          (await legacyAgent()).capacityRetry.updatedAt,
      ),
      30,
    );
    // New user input resets the schedule to the actual ten-second delay.
    await page
      .getByRole("button", { name: "Cancel automatic retry", exact: true })
      .click();
    await page
      .getByText("Automatic retry cancelled.", { exact: true })
      .waitFor();
    await page
      .locator("#message")
      .fill("A new instruction resets the retry schedule.");
    await page.locator("#send").click();
    await until(
      async () =>
        (await agent()).turnId && (await agent()).turnId !== second.turnId,
      "new user turn",
    );
    const third = await agent();
    notifyFailure(third);
    await page
      .locator('.capacity-retry[data-retry-status="scheduled"]')
      .waitFor();
    const before = (await starts()).length;
    await until(
      async () => (await starts()).length === before + 1,
      "automatic empty retry",
      16000,
    );
    assert.deepEqual((await starts()).at(-1).input, []);
    assert.equal(
      (await starts()).length,
      4,
      "exactly two user turns and two empty retries",
    );
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ passed: true, evidence: root }));
  } finally {
    fixture.stdin?.end();
  }
});
