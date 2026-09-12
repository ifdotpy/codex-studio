// Production HTTP policy and renderer. External image responses are isolated fixtures.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
const repo = new URL("../", import.meta.url).pathname;
const { chromium } = createRequire(new URL("../web/package.json", import.meta.url))("playwright-core");
const root = await mkdtemp(join(tmpdir(), "studio-image-http-"));
const fixture = spawn("python3", ["-B", join(repo, "tests/simple-ui-fixture.py"), root]);
let browser, log = "";
fixture.stderr.on("data", (data) => log += data);
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const response = await fetch(origin);
  assert.ok(response.headers.get("content-security-policy"), "Use the production content policy");
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((agent) => agent.name === "Other project");
  browser = await chromium.launch({ executablePath: process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", headless: true });
  const page = await browser.newPage();
  const requests = [];
  page.on("request", (request) => {
    if (request.url().startsWith("https://external.invalid/")) requests.push(request.url());
  });
  await page.route("https://external.invalid/**", (route) => route.fulfill({
    contentType: "image/png",
    body: Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=", "base64"),
  }));
  await page.route("**/api/sync/identity", (route) => route.fulfill({ status: 404, json: {} }));
  await page.route("**/api/transcript**", (route) => route.fulfill({ json: {
    items: [{ id: "image-message", role: "assistant", streaming: false,
      text: '![Remote](https://external.invalid/pixel.png)\n\nText <svg><image href="https://external.invalid/unapproved-svg.png" /></svg>\n\n<table background="https://external.invalid/unapproved-background.png"><tr><td>Isolated HTML</td></tr></table>',
    }], agent: lead,
  } }));
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  const load = page.getByRole("button", { name: "Load image", exact: true });
  await load.waitFor();
  await page.waitForTimeout(300);
  assert.deepEqual(requests, [], "No external image request before consent");
  await load.click();
  await page.waitForFunction(() => {
    const image = document.querySelector('img[alt="Remote"]');
    return image?.complete && image.naturalWidth === 1;
  });
  assert.deepEqual(requests, ["https://external.invalid/pixel.png"]);
  await page.getByRole("button", { name: "Preview Remote", exact: true }).click();
  await page.getByRole("dialog").waitFor();
  await page.waitForFunction(() => document.querySelector('.image-viewer img')?.naturalWidth === 1);
  console.log("PASS: production CSP permits confirmed image loads; Markdown and HTML do not load external images automatically");
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
