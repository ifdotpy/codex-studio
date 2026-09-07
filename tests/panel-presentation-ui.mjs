// Real renderer in an isolated browser. No model calls or user state.
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
const root = await mkdtemp(join(tmpdir(), "studio-panel-presentation-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  releaseRenderer = () => {},
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
  const agent = snapshot.threads.find(
    (agent) => agent.name === "Other project",
  );
  let version = 1;
  let content = {
    format: "json-render",
    spec: {
      root: "text",
      elements: {
        text: { type: "Text", props: { text: "Compact panel" }, children: [] },
      },
    },
    html: "",
    css: "",
  };
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Snapshot fixture" } }),
  );
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch(),
      state = await response.json();
    for (const item of [...state.threads, ...state.runtime.agents])
      if (item.id === agent.id) item.panelVersion = version;
    return route.fulfill({ json: state });
  });
  await page.route("**/api/panel?*", (route) =>
    route.fulfill({
      json: {
        agent: agent.id,
        version,
        ...content,
        callbacks: [],
        updated: Date.now(),
      },
    }),
  );
  const gate = new Promise((resolve) => {
    releaseRenderer = resolve;
  });
  await page.route("**/assets/panel-ui.js", async (route) => {
    await gate;
    await route.continue();
  });
  await page.goto(origin);
  const panel = page.locator(".agent-panel");
  const frame = panel.frameLocator("iframe");
  await frame.locator("#panel-root").waitFor({ state: "attached" });
  assert.equal(
    await panel
      .locator("iframe")
      .evaluate((el) => getComputedStyle(el).opacity),
    "0",
    "The unready frame stays hidden",
  );
  assert.ok(
    (await panel.boundingBox()).height <= 1,
    "The empty frame must not reserve 150px before it appears",
  );
  assert.equal(
    await frame
      .locator("html")
      .evaluate((el) => getComputedStyle(el).backgroundColor),
    "rgb(27, 27, 32)",
    "Dark background exists before the renderer loads",
  );
  assert.equal(
    await frame
      .locator("body")
      .evaluate((el) => getComputedStyle(el).backgroundColor),
    "rgb(27, 27, 32)",
  );
  assert.notEqual(
    await panel
      .locator("iframe")
      .evaluate((el) => getComputedStyle(el).transitionDuration),
    "0s",
  );
  releaseRenderer();
  const settled = async (expected) => {
    await page.waitForFunction((expected) => {
      const panel = document.querySelector(".agent-panel");
      const height = panel?.getBoundingClientRect().height;
      return (
        panel?.dataset.ready === "true" &&
        height > 0 &&
        Math.abs(height - parseFloat(panel.style.height)) < 0.5 &&
        getComputedStyle(panel.querySelector("iframe")).opacity === "1" &&
        (expected === undefined || Math.abs(height - expected) < 0.5)
      );
    }, expected);
    return (await panel.boundingBox()).height;
  };
  await frame.getByText("Compact panel", { exact: true }).waitFor();
  const compact = await settled();
  assert.ok(
    compact >= 20 && compact < 80,
    `Short structured panel: ${compact}`,
  );
  await page.screenshot({ path: join(root, "compact.png") });
  const show = async (height, text) => {
    content = {
      html: `<div style="height:${height}px">${text}</div>`,
      css: "",
    };
    version++;
    await frame.getByText(text, { exact: true }).waitFor();
    await settled(Math.min(150, height));
  };
  await show(48, "Short HTML");
  await show(240, "Tall HTML");
  await show(36, "Small again");
  const handle = await panel.locator("iframe").elementHandle();
  await frame.locator("div").evaluate((el) => {
    el.style.height = "110px";
  });
  await settled(110);
  await frame.locator("div").evaluate((el) => {
    el.style.height = "28px";
  });
  await settled(28);
  assert.equal(
    await handle.evaluate(
      (el) => el === document.querySelector(".agent-panel iframe"),
    ),
    true,
    "Resize preserves the frame",
  );
  const channel = await frame
    .locator("script[data-config]")
    .evaluate(
      (el) =>
        JSON.parse(decodeURIComponent(el.getAttribute("data-config"))).channel,
    );
  await page.evaluate(
    (channel) =>
      window.postMessage({ type: "panel-size", channel, height: 1 }, "*"),
    channel,
  );
  await frame.locator("body").evaluate((_, channel) => {
    parent.postMessage(
      { type: "panel-size", channel: "obsolete", height: 1 },
      "*",
    );
    for (const height of [-1, NaN, Infinity, "1"])
      parent.postMessage({ type: "panel-size", channel, height }, "*");
  }, channel);
  await page.waitForTimeout(250);
  assert.equal((await panel.boundingBox()).height, 28);
  await page.emulateMedia({ reducedMotion: "reduce" });
  assert.equal(
    await panel
      .locator("iframe")
      .evaluate((el) => getComputedStyle(el).transitionDuration),
    "0s",
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS: dark startup, ready fade, structured height ${compact}px, HTML shrink/grow, 150px cap, message checks, reduced motion. ${root}`,
  );
} finally {
  releaseRenderer();
  if (browser) await browser.close();
  fixture.stdin.end();
  await new Promise((resolve) => {
    fixture.once("exit", resolve);
    setTimeout(() => {
      fixture.kill("SIGTERM");
      resolve();
    }, 3000).unref();
  });
}
