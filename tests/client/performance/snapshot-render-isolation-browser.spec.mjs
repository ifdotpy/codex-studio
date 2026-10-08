import { readTestState, test, expect, spawnFixture } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("snapshot updates isolate the conversation and unchanged rows", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-snapshot-renders-"));
  const fixture = spawnFixture(
    process.env.CODEX_AGENTS_PYTHON || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), evidence],
    {
      env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "3" },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let log = "";
  fixture.stderr.on("data", (data) => (log += data));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await readTestState(origin);
  const lead = snapshot.threads.find((agent) => agent.name === "Release lead");
  const other = snapshot.threads.find(
    (agent) => agent.name === "Other project",
  );
  const workers = snapshot.threads.filter(
    (agent) => agent.rootId === lead.id && !agent.isLead,
  );
  const page = await browser.newPage({
    viewport: { width: 1600, height: 1000 },
  });
  page.setDefaultTimeout(15_000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    window.renderCounts = {};
    window.__studioPromptComposerRenderProbe = (component, id) => {
      const key = id ? `${component}:${id}` : component;
      window.renderCounts[key] = (window.renderCounts[key] || 0) + 1;
    };
  });
  const rename = async (agent, name) => {
    fixture.stdin.write(
      JSON.stringify({
        id: `rename:${agent.id}:${name}`,
        method: "fixture/entity-change",
        params: { operation: "rename", agent: agent.id, name },
      }) + "\n",
    );
    await expect(
      page
        .locator(`[data-chat="${agent.id}"], [data-worker="${agent.id}"]`)
        .first(),
    ).toContainText(name);
  };
  const counts = () => page.evaluate(() => ({ ...window.renderCounts }));
  const reset = () =>
    page.evaluate(() => {
      window.renderCounts = {};
    });
  try {
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    if (!(await page.locator(`[data-worker="${workers[0].id}"]`).isVisible()))
      await page.locator("#team-toggle").click();
    await page.locator(`[data-worker="${workers[0].id}"]`).waitFor();
    await page
      .locator("#message")
      .fill("Keep this draft while other agents update.");
    await page.waitForTimeout(700);
    await reset();
    for (let i = 0; i < 6; i++) await rename(other, `Other changed ${i}`);
    const unrelated = await counts();
    console.log(JSON.stringify({ unrelated }));
    expect(unrelated.conversation || 0).toBe(0);
    expect(unrelated[`prompt-composer:${lead.id}`] || 0).toBe(0);
    expect(unrelated[`turn-history:${lead.id}`] || 0).toBe(0);
    expect(unrelated[`sidebar-row:${lead.id}`] || 0).toBe(0);
    for (const worker of workers)
      expect(unrelated[`team-row:${worker.id}`] || 0).toBe(0);
    expect(unrelated[`sidebar-row:${other.id}`]).toBeGreaterThan(0);
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft while other agents update.",
    );
    await page.context().setOffline(true);
    await page.evaluate(() => window.dispatchEvent(new Event("offline")));
    await expect(page.locator("#error")).toContainText("Offline");
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft while other agents update.",
    );
    await page.context().setOffline(false);
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await expect(page.locator("#error")).toHaveCount(0);

    await reset();
    await rename(workers[0], "Worker changed");
    const affected = await counts();
    expect(affected[`team-row:${workers[0].id}`]).toBeGreaterThan(0);
    for (const worker of workers.slice(1))
      expect(affected[`team-row:${worker.id}`] || 0).toBe(0);
    await reset();
    await rename(lead, "Selected lead changed");
    await expect(page.locator("#conversation-title")).toContainText(
      "Selected lead changed",
    );
    const selected = await counts();
    expect(selected.conversation).toBeGreaterThan(0);
    expect(selected[`sidebar-row:${lead.id}`]).toBeGreaterThan(0);
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft while other agents update.",
    );
    await page.locator(`[data-worker="${workers[0].id}"]`).click();
    await expect(page.locator("#conversation-title")).toContainText(
      "Worker changed",
    );
    await expect(page.locator("#message")).toHaveValue("");
    await page.route("**/api/messages", (route) =>
      route.fulfill({
        json: { id: route.request().postDataJSON().id, status: "sent" },
      }),
    );
    await page.locator("#message").fill("Instruction for the selected worker.");
    const sent = page.waitForRequest(
      (request) =>
        request.method() === "POST" && request.url().endsWith("/api/messages"),
    );
    await page.locator("#send").click();
    const submitted = (await sent).postDataJSON();
    expect(submitted.room).toBe(workers[0].id);
    expect(submitted.text).toBe("Instruction for the selected worker.");
    await expect(page.locator("#message")).toHaveValue("");
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft while other agents update.",
    );
    const creation = page.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        response.url().endsWith("/api/leads"),
    );
    await page.getByRole("button", { name: "New chat", exact: true }).click();
    const created = await (await creation).json();
    await expect(page.locator(`[data-chat="${created.id}"]`)).toHaveAttribute(
      "aria-current",
      "true",
    );
    await expect(page.locator("#message")).toHaveValue("");
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft while other agents update.",
    );
    expect(errors).toEqual([]);
    console.log(JSON.stringify({ affected, selected }));
  } finally {
    await page.close();
  }
});
