import { readTestState, test, expect, spawnFixture } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("snapshot updates isolate the conversation and unchanged rows", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-snapshot-renders-"));
  const fixture = spawnFixture(
    process.env.CODEX_AGENTS_PYTHON || "python3",
    [
      "-B",
      join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      evidence,
    ],
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
  const pendingReads = new Set();
  const completedTranscripts = new Set();
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/api/") && url.pathname !== "/api/sync/stream")
      pendingReads.add(request);
  });
  page.on("requestfinished", (request) => {
    pendingReads.delete(request);
    const url = new URL(request.url());
    if (url.pathname === "/api/sync/pull")
      completedTranscripts.add(url.searchParams.get("scope"));
  });
  page.on("requestfailed", (request) => pendingReads.delete(request));
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    window.resourceFrames = [];
    const NativeEventSource = window.EventSource;
    window.EventSource = class extends NativeEventSource {
      constructor(...args) {
        super(...args);
        window.activeResourceStream = this;
        this.addEventListener("resources", (event) => {
          window.resourceFrames.push(JSON.parse(event.data));
          window.latestResourceFrameSource = this;
        });
      }
    };
    window.renderCounts = {};
    window.modelWork = {};
    window.__studioSidebarModelProbe = (kind, count) => {
      window.modelWork[kind] = (window.modelWork[kind] || 0) + count;
    };
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
  const frames = () => page.evaluate(() => window.resourceFrames);
  const modelWork = () => page.evaluate(() => ({ ...window.modelWork }));
  const reset = () =>
    page.evaluate(() => {
      window.renderCounts = {};
      window.modelWork = {};
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
    // The idle prefetch pump mounts transcript subscriptions after 1000 ms.
    // Wait for their stream baseline and completed reads before measuring
    // entity changes. Require the current source because an older baseline
    // can precede a failed replacement stream and its pending reconnect.
    await expect
      .poll(() =>
        page.evaluate(
          (ids) => {
            const event = window.resourceFrames.at(-1);
            return (
              window.activeResourceStream?.readyState === EventSource.OPEN &&
              window.latestResourceFrameSource ===
                window.activeResourceStream &&
              event?.reason === "initial" &&
              ids.every((id) =>
                event.resources.some(
                  (resource) =>
                    resource.kind === "transcript" && resource.agentId === id,
                ),
              )
            );
          },
          snapshot.threads.map((agent) => agent.id),
        ),
      )
      .toBe(true);
    await expect
      .poll(() =>
        snapshot.threads.every((agent) =>
          completedTranscripts.has(`transcript:${agent.id}`),
        ),
      )
      .toBe(true);
    await expect.poll(() => pendingReads.size).toBe(0);
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    await reset();
    await page.waitForTimeout(200);
    const idle = await counts();
    expect(idle.conversation || 0).toBe(0);
    for (const worker of workers)
      expect(idle[`team-row:${worker.id}`] || 0).toBe(0);
    await reset();
    const firstMeasuredFrame = (await frames()).length;
    for (let i = 0; i < 6; i++) await rename(other, `Other changed ${i}`);
    const unrelated = await counts();
    const unrelatedModelWork = await modelWork();
    expect(unrelatedModelWork["catalog-row"]).toBeGreaterThan(0);
    expect(unrelatedModelWork["order-compare"] || 0).toBe(0);
    expect(unrelatedModelWork["search-row"] || 0).toBe(0);
    console.log(JSON.stringify({ idle, unrelated, unrelatedModelWork }));
    // No baseline or reconnect can explain renders in this measured interval.
    const measuredFrames = (await frames()).slice(firstMeasuredFrame);
    expect(measuredFrames.length).toBeGreaterThan(0);
    for (const frame of measuredFrames) {
      expect(frame.reason).toBe("change");
      expect(
        frame.resources.every((resource) => resource.kind === "state"),
      ).toBe(true);
    }
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
    await reset();
    await rename(workers[0], "Worker changed");
    const affected = await counts();
    const workerModelWork = await modelWork();
    expect(workerModelWork["catalog-row"]).toBeGreaterThan(0);
    expect(workerModelWork["order-compare"] || 0).toBe(0);
    expect(workerModelWork["search-row"] || 0).toBe(0);
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
    // Reconnect intentionally refreshes catalogs and transcript resources.
    // Check that behavior after the isolated entity render measurements.
    await page.context().setOffline(true);
    await page.evaluate(() => window.dispatchEvent(new Event("offline")));
    await expect(page.locator("#error")).toContainText("Offline");
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft while other agents update.",
    );
    await page.context().setOffline(false);
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await expect(page.locator("#error")).toHaveCount(0);
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
