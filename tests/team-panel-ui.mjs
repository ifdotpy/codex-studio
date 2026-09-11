#!/usr/bin/env node
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-team-panel-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  const errors = [],
    actions = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/action", (route) => {
    actions.push(route.request().postDataJSON());
    return route.fulfill({ json: { status: "accepted" } });
  });
  const lead = snapshot.threads.find((agent) => agent.name === "Release lead");
  const member = snapshot.threads.find(
    (agent) => agent.rootId === lead.id && !agent.isLead,
  );
  const rawError =
    "Command ['git', '-C', '/workspace/project', 'worktree', 'add', '-b', 'codex-agent/" +
    "12345678-".repeat(8) +
    "', '/workspace/project/.worktrees/codex-agents/" +
    "12345678-".repeat(8) +
    "', 'HEAD'] returned non-zero exit status 128.";
  Object.assign(member, {
    name: "Подача уведомления",
    status: "failed",
    inFlight: false,
    error: rawError,
    overview: {
      task: "Research current official government sources for filing a notification.",
      result: "",
    },
  });
  snapshot.threads = [lead, member];
  snapshot.runtime.agents = [lead, member];
  snapshot.runtime.requests = [];
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({
      status: 503,
      json: { error: "Fixture uses HTTP snapshots" },
    }),
  );
  await page.route("**/api/state", (route) =>
    route.fulfill({ json: snapshot }),
  );
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.locator("#team-toggle").click();
  for (const width of [1440, 320]) {
    await page.setViewportSize({ width, height: 960 });
    await page.emulateMedia({ colorScheme: width === 320 ? "dark" : "light" });
    if (!(await page.locator("#team").isVisible()))
      await page.locator("#team-toggle").click();
    const panel = page.locator("#team");
    assert.equal(
      await panel
        .locator(".team-overview, #worker-search, .team-filters")
        .count(),
      0,
    );
    assert.match(
      await panel.locator(".team-heading").innerText(),
      /1 subagent\b/,
    );
    assert.equal(
      await panel.locator(".worker-error").innerText(),
      "Could not prepare the project folder.",
    );
    const card = panel.locator(".worker-entry");
    assert.ok((await card.boundingBox()).height < 250, "compact error card");
    assert.equal(await card.locator("pre").isVisible(), false);
    await panel.screenshot({ path: join(evidence, `team-${width}.png`) });
    await card.locator(".worker-error-details summary").click();
    assert.equal(
      await card.locator("pre").innerText(),
      rawError,
      "full original error remains available",
    );
    assert.ok(await card.locator("pre").isVisible());
    assert.ok(
      await panel.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
      "no horizontal overflow",
    );
    await card.locator(".worker-error-details summary").click();
  }
  assert.deepEqual(errors, []);
  console.log(
    `PASS compact team, readable error, complete details, 1440/320px. ${evidence}`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
