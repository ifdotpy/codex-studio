#!/usr/bin/env node
// Desktop alert reads remain small and cannot overlap while Limits is open.
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
const evidence = await mkdtemp(join(tmpdir(), "studio-workspace-inbox-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (value) => {
  log += value;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (value) =>
      resolve(Number(String(value).trim())),
    );
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state?view=chat")).json();
  const lead = snapshot.threads.find((row) => row.name === "Release lead");
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const context = await browser.newContext({
    viewport: { width: 1280, height: 900 },
  });
  await context.grantPermissions(["notifications"], { origin });
  await context.addInitScript(
    ({ stateDir, id }) => {
      localStorage.setItem("workspace-notifications", "true");
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
    },
    { stateDir: snapshot.stateDir, id: lead.id },
  );
  page = await context.newPage();
  const errors = [],
    pending = [],
    requests = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/workspace?*", (route) => {
    const url = new URL(route.request().url());
    assert.equal(url.searchParams.get("view"), "inbox");
    assert.equal(url.searchParams.get("agent"), lead.id);
    requests.push(Date.now());
    pending.push(route);
  });
  await page.route("**/api/costs?*", (route) =>
    route.fulfill({
      json: {
        accountKey: "default",
        refreshing: false,
        stale: false,
        data: { todayUSD: 12.5, last30DaysUSD: 1234.56, coverage: "reported" },
      },
    }),
  );
  await page.goto(origin);
  await page
    .getByRole("button", { name: "Account limits", exact: true })
    .click();
  const details = page.getByRole("region", {
    name: "Account limits details",
    exact: true,
  });
  await details.getByText("API cost estimate", { exact: true }).waitFor();
  await details.getByText("$12.50", { exact: true }).waitFor();
  await page.waitForTimeout(11000);
  assert.equal(
    requests.length,
    1,
    "a slow alert read cannot start another alert read",
  );
  for (const route of pending.splice(0)) {
    const response = await route.fetch();
    const result = await response.json();
    assert.deepEqual(Object.keys(result), ["inbox"]);
    await route.fulfill({ response });
  }
  assert.doesNotMatch(
    await details.innerText(),
    /server did not respond in time/,
  );
  assert.deepEqual(errors, []);
  await page.screenshot({
    path: join(evidence, "limits-with-pending-inbox.png"),
  });
  console.log(
    JSON.stringify({ passed: true, evidence, inboxRequests: requests.length }),
  );
} catch (error) {
  await page?.screenshot({ path: join(evidence, "failure.png") });
  console.error(evidence, log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
