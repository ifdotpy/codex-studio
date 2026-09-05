#!/usr/bin/env node
// Real runtime notification handlers, SQLite, SSE and Chromium. No inference.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-rich-preview-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn, label) => {
  for (let i = 0; i < 200; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = async () => await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const context = await browser.newContext({
    viewport: { width: 1280, height: 960 },
    permissions: ["clipboard-read", "clipboard-write"],
  });
  page = await context.newPage();
  const errors = [],
    forbiddenRequests = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (/preview-escape|evil\.invalid/.test(r.url())) forbiddenRequests.push(r);
  });
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Render the preview fixtures");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    agent = (await state()).runtime.agents.find(
      (a) => a.name === "Other project",
    );
    return agent.status === "running" && agent.turnId;
  }, "turn starts");
  const event = (method, params) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
      }) + "\n",
    );
  event("item/started", {
    item: { id: "rich-stream", type: "agentMessage", text: "" },
  });
  event("item/agentMessage/delta", {
    itemId: "rich-stream",
    delta:
      "Diagram follows.\n\n```mermaid\nflowchart LR\n A[Lead] --> B[Worker]\n\n",
  });
  await page.getByText("Diagram follows.", { exact: true }).waitFor();
  assert.equal(
    await page.locator(".rich-preview").count(),
    0,
    "incomplete Mermaid fence stays buffered",
  );
  event("item/agentMessage/delta", { itemId: "rich-stream", delta: "```\n\n" });
  const diagram = page.locator('[data-preview-kind="mermaid"]').first();
  const diagramFrame = diagram.frameLocator("iframe");
  await diagramFrame.locator("svg").waitFor();
  assert.match(await diagramFrame.locator("body").textContent(), /Lead/);
  await diagram
    .locator("iframe")
    .evaluate((el) => (el.dataset.retained = "yes"));
  event("item/agentMessage/delta", {
    itemId: "rich-stream",
    delta: "```html\n<style>body{color:red}</style>\n\n",
  });
  await page.waitForTimeout(200);
  assert.equal(
    await page.locator('[data-preview-kind="html"]').count(),
    0,
    "incomplete HTML fence stays buffered",
  );
  const html = `<html class="preview-root"><head><style>body.demo{background:rgb(222, 241, 230)}html.preview-root .box{border:3px solid rgb(30, 70, 90)}.box{padding:24px;background:rgb(12, 85, 60);color:white;border-radius:14px}</style><meta http-equiv="refresh" content="0;url=https://evil.invalid/preview-escape"></head><body class="demo" style="color:rgb(4,5,6)"><div class="box"><h2>Rendered HTML card</h2><svg width="90" height="40"><rect width="80" height="30" fill="orange"/></svg></div><script>parent.document.body.dataset.escape='yes';localStorage.setItem('escape','yes');fetch('/api/preview-escape')</script><a href="https://evil.invalid/preview-escape" target="_top">Escape link</a><svg width="260" height="40"><a href="#"><set attributeName="href" to="https://evil.invalid/smil-escape"/><text x="0" y="24">SMIL escape link</text></a><animate attributeName="opacity" from="0" to="1" dur="1s"/><animateMotion path="M0,0 L10,10" dur="1s"/><animateTransform attributeName="transform" type="rotate" from="0" to="10" dur="1s"/></svg><form action="/api/preview-escape"><button>Submit</button></form><img src="https://evil.invalid/preview-escape"><iframe src="/api/preview-escape"></iframe><div style="background-image:url(https://evil.invalid/preview-escape)">Network CSS</div></body></html>`;
  const full =
    "Diagram follows.\n\n```mermaid\nflowchart LR\n A[Lead] --> B[Worker]\n\n```\n\n```html\n" +
    html +
    "\n```\n\n```mermaid\nthis is invalid Mermaid syntax !!!\n```\n\n<div><strong>Raw HTML block</strong></div>\n";
  event("item/completed", {
    item: { id: "rich-stream", type: "agentMessage", text: full },
  });
  const htmlCard = page.locator('[data-preview-kind="html"]').first();
  const htmlFrame = htmlCard.frameLocator("iframe");
  await htmlFrame.getByText("Rendered HTML card").waitFor();
  assert.equal(
    await htmlFrame
      .locator(".box")
      .evaluate((el) => getComputedStyle(el).backgroundColor),
    "rgb(12, 85, 60)",
  );
  assert.equal(await htmlFrame.locator("svg rect").count(), 1);
  assert.equal(
    await htmlFrame.locator("html").getAttribute("class"),
    "preview-root",
  );
  assert.equal(await htmlFrame.locator("body").getAttribute("class"), "demo");
  assert.equal(
    await htmlFrame
      .locator("body")
      .evaluate((el) => getComputedStyle(el).color),
    "rgb(4, 5, 6)",
  );
  assert.equal(
    await htmlFrame
      .locator("body")
      .evaluate((el) => getComputedStyle(el).backgroundColor),
    "rgb(222, 241, 230)",
  );
  assert.equal(
    await htmlFrame
      .locator(".box")
      .evaluate((el) => getComputedStyle(el).borderTopWidth),
    "3px",
  );
  assert.equal(
    await htmlFrame.locator("script,iframe,meta[http-equiv=refresh]").count(),
    0,
  );
  assert.equal(await htmlCard.locator("iframe").getAttribute("sandbox"), "");
  assert.equal(
    await htmlCard.locator("iframe").evaluate((el) => el.contentDocument),
    null,
    "opaque frame cannot expose its document to the parent",
  );
  await htmlFrame.getByText("Escape link", { exact: true }).click();
  assert.equal(page.url(), origin + "/");
  assert.equal(
    await htmlFrame
      .locator("animate, set, animateMotion, animateTransform")
      .count(),
    0,
    "static previews remove SVG animation that can restore navigation attributes",
  );
  assert.equal(
    await htmlFrame.locator("svg a").getAttribute("href"),
    null,
    "SVG anchors cannot navigate to the parent base URL",
  );
  await htmlFrame.getByText("SMIL escape link", { exact: true }).click();
  await htmlFrame.getByText("Rendered HTML card").waitFor();
  assert.equal(page.url(), origin + "/");
  assert.equal(
    forbiddenRequests.filter((r) => r.url().includes("smil-escape")).length,
    0,
    "SVG link cannot restore external navigation through SMIL",
  );
  assert.equal(
    await htmlFrame
      .getByText("Escape link", { exact: true })
      .getAttribute("href"),
    null,
  );
  assert.equal(
    await page.evaluate(() => document.body.dataset.escape),
    undefined,
  );
  assert.equal(await page.evaluate(() => localStorage.getItem("escape")), null);
  const invalidDiagram = page.locator('[data-preview-kind="mermaid"]').nth(1);
  await invalidDiagram.getByRole("alert").waitFor();
  await invalidDiagram
    .getByRole("button", { name: "Source", exact: true })
    .click();
  assert.equal(
    await invalidDiagram.locator("pre code").textContent(),
    "this is invalid Mermaid syntax !!!",
  );
  await invalidDiagram
    .getByRole("button", { name: "Preview", exact: true })
    .click();
  assert.equal(
    await diagram.locator("iframe").getAttribute("data-retained"),
    "yes",
    "complete diagrams remain mounted across later stream updates",
  );
  await page
    .locator('[data-preview-kind="html"]')
    .nth(1)
    .frameLocator("iframe")
    .getByText("Raw HTML block")
    .waitFor();
  await htmlCard.getByRole("button", { name: "Source", exact: true }).click();
  assert.equal(await htmlCard.locator("pre code").textContent(), html);
  await htmlCard.getByRole("button", { name: "Copy source" }).click();
  assert.equal(await page.evaluate(() => navigator.clipboard.readText()), html);
  const downloadEvent = page.waitForEvent("download");
  await htmlCard.getByRole("link", { name: "Download preview" }).click();
  const download = await downloadEvent;
  assert.equal(download.suggestedFilename(), "preview.html");
  assert.equal(await readFile(await download.path(), "utf8"), html);
  const svgEvent = page.waitForEvent("download");
  await diagram.getByRole("link", { name: "Download preview" }).click();
  const svgDownload = await svgEvent;
  assert.equal(svgDownload.suggestedFilename(), "diagram.svg");
  assert.match(await readFile(await svgDownload.path(), "utf8"), /<svg/);
  await htmlCard.getByRole("button", { name: "Preview", exact: true }).click();
  await htmlFrame.getByText("Rendered HTML card").waitFor();
  await htmlCard.scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(root, "desktop.png"), fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  await htmlCard.scrollIntoViewIfNeeded();
  await htmlFrame.getByText("Rendered HTML card").waitFor();
  await page.screenshot({ path: join(root, "mobile.png"), fullPage: true });
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  for (const card of await page.locator(".rich-preview").all()) {
    const box = await card.boundingBox();
    assert.ok(
      box && box.x >= 0 && box.x + box.width <= 391,
      "preview stays inside narrow viewport",
    );
  }
  assert.ok(
    forbiddenRequests.every((r) =>
      ["csp", "net::ERR_BLOCKED_BY_CSP"].includes(r.failure()?.errorText),
    ),
    "preview resource requests are blocked by CSP: " +
      JSON.stringify(
        forbiddenRequests.map((r) => ({ url: r.url(), failure: r.failure() })),
      ),
  );
  assert.deepEqual(errors, []);
  console.log(
    "Rich preview UI: PASS (stream boundaries, Mermaid SVG/error, HTML/CSS/SVG, sandbox, stable blocks, source/copy/download, mobile)",
  );
  console.log("Browser evidence:", root);
} catch (e) {
  await page?.screenshot({ path: join(root, "failure.png"), fullPage: true });
  console.error("Evidence:", root);
  throw e;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
}
