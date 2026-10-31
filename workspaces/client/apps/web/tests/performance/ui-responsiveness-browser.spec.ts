import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { standaloneCommand } from "../../src/commands.ts";
import { cacheFixture } from "../cacheFixture.ts";
import {
  readTestState,
  rustUiFixture,
  queryFixtureDatabase,
  spawnFixture,
  test,
  expect,
} from "../playwright.ts";
import { installResponsivenessFixture } from "./ui-responsiveness.fixture.ts";
import { required } from "./performance-fixture.ts";

test("composer latency with server history and streaming @performance", async ({
  browser,
}) => {
  test.setTimeout(600_000);
  const root = await mkdtemp(join(tmpdir(), "studio-composer-latency-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      FIXTURE_WORKER_COUNT: "0",
      FIXTURE_SCRIPT: JSON.stringify([
        ...Array.from({ length: 300 }, (_, i) => ({
          prompt: `Benchmark turn ${i} end`,
          steps: [
            {
              say: `Benchmark result ${i} **evidence**\n\n\`\`\`ts\nconst result = await check();\n\`\`\``,
            },
          ],
        })),
        { prompt: "Benchmark live", steps: [{ hold: true }] },
      ]),
    },
  });
  let log = "";
  required(fixture.stderr).on("data", (chunk) => {
    log += chunk;
  });
  const port = await new Promise<number>((resolve, reject) => {
    required(fixture.stdout).once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await readTestState(origin);
  const agent = required(
    state.threads.find((row) => row.name === "Other project"),
  );
  const other = required(
    state.threads.find((row) => row.name === "Release lead"),
  );
  const send = async (text: string) => {
    const response = await fetch(`${origin}/api/commands`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        Origin: origin,
        "X-Canvas-Token": state.token,
      },
      body: JSON.stringify(
        standaloneCommand("send", agent.id, { text, delivery: "next_turn" }),
      ),
    });
    expect(response.status, await response.clone().text()).toBe(200);
  };
  const context = await browser.newContext({
    viewport: { width: 1440, height: 960 },
  });
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  await page.addInitScript(installResponsivenessFixture, {
    stateDir: required(state.stateDir),
    id: agent.id,
    cachedId: cacheFixture(agent.id),
  });
  await page.goto(origin);
  await page.locator(`[data-chat="${agent.id}"]`).click();
  for (let i = 0; i < 300; i++) {
    await send(`Benchmark turn ${i} end`);
    await page
      .getByText(`Benchmark result ${i} `, { exact: false })
      .last()
      .waitFor({ timeout: 30_000 });
  }
  await page.locator(`[data-chat="${other.id}"]`).click();
  await expect(
    page.getByText("Review the release", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Benchmark turn 299 end", { exact: true }),
  ).toHaveCount(0);
  await page.locator(`[data-chat="${agent.id}"]`).click();
  await expect(
    page.getByText("Benchmark result 299 ", { exact: false }).last(),
  ).toBeVisible();
  await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
  await page.reload();
  await page
    .getByText("Benchmark result 299 ", { exact: false })
    .last()
    .waitFor({ timeout: 60_000 });
  const turns = queryFixtureDatabase(
    root,
    "SELECT state, count(*) AS count FROM turns WHERE chat=? GROUP BY state",
    agent.id,
  );
  console.log("SERVER_TURNS", JSON.stringify(turns));
  expect(
    turns.find((row) => row.state === "completed")?.count,
  ).toBeGreaterThanOrEqual(300);
  await page.evaluate(() => {
    window.metrics.phase = "idle-control";
  });
  await page.waitForTimeout(1200);
  const input = page.locator("#message");
  await input.focus();
  await page.evaluate(() => {
    window.metrics.phase = "idle-input";
  });
  await page.keyboard.type("Typing with a long transcript.", { delay: 35 });
  await page.waitForTimeout(150);
  await send("Benchmark live");
  await page
    .getByText("Benchmark live", { exact: true })
    .waitFor({ timeout: 60_000 });
  await page.locator("#stop").waitFor({ timeout: 60_000 });
  await page.evaluate(() => {
    window.metrics.phase = "stream-and-input";
  });
  const stream = async () => {
    for (let i = 0; i < 20; i++) {
      required(fixture.stdin).write(
        JSON.stringify({
          method: "fixture/provider",
          agent: agent.id,
          steps: [
            { item: "latency-stream", delta: ` token ${i}`, append: i > 0 },
          ],
        }) + "\n",
      );
      await new Promise((resolve) => setTimeout(resolve, 70));
    }
  };
  await Promise.all([
    stream(),
    page.keyboard.type(" Typing while tokens arrive.", { delay: 35 }),
  ]);
  await expect(page.getByText(/token 19/).last()).toBeVisible();
  await page.waitForTimeout(300);
  await page.evaluate(() => {
    window.metrics.phase = "stream-only";
  });
  required(fixture.stdin).write(
    JSON.stringify({
      method: "fixture/provider",
      agent: agent.id,
      steps: [
        { item: "latency-stream", delta: " stream-only-marker", append: true },
      ],
    }) + "\n",
  );
  await expect(page.getByText(/stream-only-marker/).last()).toBeVisible();
  await page.waitForTimeout(150);
  const metrics = await page.evaluate(() => window.metrics);
  for (const phase of ["idle-input", "stream-and-input"]) {
    const durations = metrics.input
      .filter((entry) => entry.phase === phase)
      .map((entry) => entry.duration)
      .sort((a, b) => a - b);
    const renders = Object.fromEntries(
      ["app", "sidebar", "conversation", "prompt-composer"].map((component) => [
        component,
        metrics.renderProbe.filter(
          (entry) => entry.phase === phase && entry.component === component,
        ).length / durations.length,
      ]),
    );
    const nearInputRenders = Object.fromEntries(
      ["app", "sidebar", "conversation", "prompt-composer"].map((component) => [
        component,
        metrics.renderProbe.filter(
          (entry) =>
            entry.phase === phase &&
            entry.component === component &&
            entry.inputAgeMs < 20,
        ).length,
      ]),
    );
    console.log(
      JSON.stringify({
        phase,
        keys: durations.length,
        p50: durations[Math.floor(durations.length * 0.5)],
        p95: durations[Math.floor(durations.length * 0.95)],
        rendersPerKey: renders,
        nearInputRenders,
      }),
    );
    expect(durations.length).toBeGreaterThan(20);
  }
  console.log(
    "IDLE_CONTROL_RENDERS",
    JSON.stringify(
      metrics.renderProbe.filter((entry) => entry.phase === "idle-control"),
    ),
  );
  expect(
    metrics.renderProbe.filter(
      (entry) =>
        entry.phase === "stream-only" && entry.component === "prompt-composer",
    ),
  ).toHaveLength(0);
  await context.close();
});
