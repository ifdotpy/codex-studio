import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const cacheDir = await mkdtemp(join(tmpdir(), "studio-progress-fit-"));
const entry = join(root, "progress-fit-fixture.tsx");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  optimizeDeps: {
    noDiscovery: true,
    include: [
      "react",
      "react/jsx-runtime",
      "react/jsx-dev-runtime",
      "react-dom",
      "react-dom/client",
      "@mantine/core",
      "marked",
      "dompurify",
    ],
  },
  plugins: [
    {
      name: "progress-fit-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/progress-fit-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/progress-fit-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `
import React,{useState} from 'react';import{createRoot}from'react-dom/client';import{flushSync}from'react-dom';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import AgentPanel from'/src/components/AgentPanel.tsx';import{setToken}from'/src/api.ts';import'/src/studio-theme.css';setToken('fixture-token');
function Fixture(){const[agent,setAgent]=useState('first');window.switchAgent=id=>flushSync(()=>setAgent(id));return <MantineProvider><div id='host' style={{width:'100%',maxWidth:520}}><AgentPanel agentId={agent} stateDir='/workspace'/></div></MantineProvider>}createRoot(document.getElementById('root')).render(<Fixture/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await engine.launch({
    headless: true,
    ...(engine === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({ viewport: { width: 540, height: 800 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    let markdown = "# Stage\n\nReady.",
      revision = "1",
      hidden = false,
      online = true,
      offset = 0,
      mode = "ok";
    const now = Date.now;
    Date.now = () => now() + offset;
    Object.defineProperty(document, "hidden", {
      get: () => hidden,
      configurable: true,
    });
    Object.defineProperty(navigator, "onLine", {
      get: () => online,
      configurable: true,
    });
    window.visibility = (value) => {
      hidden = value;
      document.dispatchEvent(new Event("visibilitychange"));
    };
    window.online = (value) => {
      online = value;
      window.dispatchEvent(new Event(value ? "online" : "offline"));
    };
    window.advance = () => {
      offset += 31000;
      window.dispatchEvent(new Event("pageshow"));
    };
    window.changeProgress = (text) => {
      markdown = text;
      revision = String(Number(revision) + 1);
      window.dispatchEvent(new Event("pageshow"));
      return revision;
    };
    window.reportMode = (value) => {
      mode = value;
    };
    window.reports = [];
    window.fileReads = [];
    window.activeReports = 0;
    window.maxActiveReports = 0;
    const original = window.fetch;
    window.fetch = (input, options = {}) => {
      const url = new URL(String(input), location.href);
      if (url.pathname === "/api/panel")
        return Promise.resolve(
          new Response(
            JSON.stringify({
              agent: url.searchParams.get("agent"),
              format: "markdown",
              markdown,
              revision,
              path:
                "/workspace/progress/" +
                url.searchParams.get("agent") +
                "/PROGRESS.md",
              error: null,
            }),
            { status: 200 },
          ),
        );
      if (url.pathname === "/api/file") {
        window.fileReads.push(Object.fromEntries(url.searchParams));
        return Promise.resolve(
          new Response(
            JSON.stringify({
              name: "PROGRESS.md",
              mime: "text/markdown",
              base64: btoa(markdown),
            }),
            { status: 200 },
          ),
        );
      }
      if (url.pathname === "/api/panel/layout") {
        const report = {
          body: JSON.parse(options.body),
          headers: options.headers,
          aborted: false,
          mode,
        };
        window.reports.push(report);
        window.activeReports++;
        window.maxActiveReports = Math.max(
          window.maxActiveReports,
          window.activeReports,
        );
        const response = () => {
          window.activeReports--;
          return new Response(
            JSON.stringify(
              mode === "failed" ? { error: "fixture report failed" } : {},
            ),
            { status: mode === "failed" ? 503 : mode === "stale" ? 409 : 200 },
          );
        };
        if (mode === "hold")
          return new Promise((resolve, reject) => {
            window.releaseReport = () => {
              window.releaseReport = null;
              resolve(response());
            };
            options.signal.addEventListener(
              "abort",
              () => {
                report.aborted = true;
                window.activeReports--;
                window.releaseReport = null;
                reject(new DOMException("Aborted", "AbortError"));
              },
              { once: true },
            );
          });
        return Promise.resolve(response());
      }
      return original(input, options);
    };
  });
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/check`,
    { waitUntil: "commit" },
  );
  const panel = page.getByRole("region", { name: "Agent progress" }),
    current = panel.locator(".agent-panel-current");
  await current.getByText("Ready.", { exact: true }).waitFor();
  await page.waitForFunction(() => reports.at(-1)?.body.fits === true);
  const report = await page.evaluate(() => reports.at(-1));
  assert.equal(report.headers["X-Canvas-Token"], "fixture-token");
  assert.deepEqual(
    Object.keys(report.body).sort(),
    [
      "agent",
      "client",
      "sequence",
      "renderer",
      "revision",
      "width",
      "height",
      "contentWidth",
      "contentHeight",
      "fits",
      "reason",
    ].sort(),
  );
  assert.equal(report.body.renderer, "progress-markdown-v1");
  assert.match(report.body.client, /^[\w-]{36}$/);
  assert.equal(report.body.reason, null);
  assert(
    report.body.width > 0 && report.body.height > 0 && report.body.height < 150,
  );
  const change = async (text) => {
    const revision = await page.evaluate(
      (value) => window.changeProgress(value),
      text,
    );
    await page.waitForFunction(
      (revision) => reports.at(-1)?.body.revision === revision,
      revision,
    );
    return revision;
  };
  await page.evaluate(() => {
    document.getElementById("host").style.width = "0px";
  });
  await page.waitForTimeout(80);
  const zero = await page.evaluate(() => {
    window.advance();
    return reports.length;
  });
  await page.waitForTimeout(100);
  assert.equal(
    await page.evaluate(() => reports.length),
    zero,
    "Zero available dimensions remain unmeasured",
  );
  await page.evaluate(() => {
    document.getElementById("host").style.width = "100%";
  });
  await current.waitFor();
  await change("Literal `&lt;tag&gt;` and **strong**.");
  assert.equal(await current.locator("code").innerText(), "&lt;tag&gt;");
  await page.addStyleTag({
    content:
      ".progress-markdown{font-size:14.7px!important;line-height:1.35!important}.agent-panel-heading{line-height:16.25px!important}",
  });
  await change(Array.from({ length: 6 }, (_, i) => `Line ${i}.`).join("  \n"));
  const fractional = await page.evaluate(() => reports.at(-1).body);
  assert.equal(fractional.fits, true);
  assert.notEqual(fractional.height % 1, 0);
  assert(fractional.contentHeight <= fractional.height + 0.5);
  await page.addStyleTag({
    content:
      ".progress-markdown{font-size:12px!important}.agent-panel-heading{line-height:16px!important}",
  });
  await change("1. Ready\n2. Review");
  assert.equal(await current.locator("ol").count(), 1);
  await change(
    "999999999. Long ordered marker with text that must remain inside the candidate bounds.",
  );
  await page.setViewportSize({ width: 190, height: 800 });
  await page.waitForFunction(() => reports.at(-1)?.body.width < 200);
  assert.equal(
    await panel
      .locator(".agent-panel-measure ol")
      .evaluate((el) => getComputedStyle(el).listStylePosition),
    "inside",
  );
  const marker = await page.evaluate(() => reports.at(-1).body);
  if (marker.fits)
    assert(
      marker.contentWidth <= marker.width + 0.5 &&
        marker.contentHeight <= marker.height + 0.5,
    );
  await page.setViewportSize({ width: 540, height: 800 });
  const wrapping =
    "A measured progress sentence that wraps when the available width becomes smaller. ".repeat(
      4,
    );
  await change(wrapping);
  await page.waitForFunction(() => reports.at(-1)?.body.fits === true);
  await page.setViewportSize({ width: 210, height: 800 });
  await panel.getByText("Progress does not fit.", { exact: true }).waitFor();
  assert.equal(await current.count(), 0);
  const narrow = await page.evaluate(() => reports.at(-1).body);
  assert.equal(narrow.reason, "overflow");
  assert(
    narrow.contentHeight > narrow.height || narrow.contentWidth > narrow.width,
  );
  assert.equal(
    await panel
      .locator(".agent-panel-content")
      .evaluate(
        (el) =>
          el.scrollHeight > el.clientHeight || el.scrollWidth > el.clientWidth,
      ),
    false,
  );
  await page.setViewportSize({ width: 540, height: 800 });
  await current.waitFor();
  await change("First line.  \nSecond line.  \nThird line.  \nFourth line.");
  await page.setViewportSize({ width: 540, height: 270 });
  await panel.getByText("Progress does not fit.", { exact: true }).waitFor();
  assert.equal(
    await panel
      .locator(".agent-panel-measure .progress-markdown")
      .evaluate((el) => getComputedStyle(el).fontSize),
    "12px",
  );
  assert((await panel.boundingBox()).height <= 80.5);
  await page.setViewportSize({ width: 540, height: 800 });
  await change("Measured font change. ".repeat(8));
  await current.waitFor();
  await page.addStyleTag({
    content: ".progress-markdown{font-size:24px!important}",
  });
  await panel.getByText("Progress does not fit.", { exact: true }).waitFor();
  await page.addStyleTag({
    content: ".progress-markdown{font-size:12px!important}",
  });
  await current.waitFor();
  for (const unsupported of [
    "![image](https://invalid.example/x.png)",
    "```html\n<button>Go</button>\n```",
    "| A | B |\n| - | - |\n| x | y |",
    "<form><input></form>",
    "[bad](javascript:alert(1))",
  ]) {
    await change(unsupported);
    await panel
      .getByText("Progress format is unsupported.", { exact: true })
      .waitFor();
    assert.equal(await current.count(), 0);
    assert.equal(
      (await page.evaluate(() => reports.at(-1).body)).reason,
      "unsupported",
    );
    assert.equal(await panel.locator("img,iframe,form,input").count(), 0);
  }
  const long = Array.from(
    { length: 50 },
    (_, i) => `- Complete step ${i}`,
  ).join("\n");
  await change(long);
  await panel.getByText("Progress does not fit.", { exact: true }).waitFor();
  await panel.getByRole("button", { name: "Open PROGRESS.md" }).click();
  const fileDialog = page.getByRole("dialog", {
    name: "PROGRESS.md",
    exact: true,
  });
  await fileDialog.waitFor();
  await fileDialog.getByRole("button", { name: "Source", exact: true }).click();
  assert.equal(await fileDialog.locator("pre").innerText(), long);
  await page.keyboard.press("Escape");
  await fileDialog.waitFor({ state: "hidden" });
  await change("[Result](../../project/report.txt)");
  await current.getByRole("link", { name: "Result" }).click();
  await page.waitForFunction(
    () =>
      fileReads.at(-1)?.path ===
      "/workspace/progress/first/../../project/report.txt",
  );
  await page.keyboard.press("Escape");
  await change("Recovery.");
  await current.getByText("Recovery.", { exact: true }).waitFor();
  const beforeHeartbeat = await page.evaluate(() => reports.length);
  await page.evaluate(() => window.advance());
  await page.waitForFunction(
    (count) => reports.length > count,
    beforeHeartbeat,
  );
  const heartbeat = await page.evaluate(() =>
    reports.slice(-2).map((item) => item.body),
  );
  assert.equal(heartbeat[0].client, heartbeat[1].client);
  assert(heartbeat[1].sequence > heartbeat[0].sequence);
  await page.evaluate(() => window.reportMode("failed"));
  await change("Failure source.");
  await panel
    .getByRole("button", { name: "Cannot report panel size" })
    .waitFor();
  await page.evaluate(() => window.reportMode("ok"));
  await panel
    .getByRole("button", { name: "Cannot report panel size" })
    .waitFor({ state: "hidden", timeout: 3000 });
  await page.evaluate(() => window.reportMode("failed"));
  await change("Another failure.");
  await panel
    .getByRole("button", { name: "Cannot report panel size" })
    .waitFor();
  const beforeNarrowFailure = await page.evaluate(() => reports.length);
  await page.setViewportSize({ width: 190, height: 800 });
  await page.waitForTimeout(500);
  assert.equal(
    await panel
      .getByRole("button", { name: "Cannot report panel size" })
      .count(),
    1,
  );
  assert(
    await page.evaluate(
      (count) => reports.length - count < 6,
      beforeNarrowFailure,
    ),
    "A wrapped report-error label must not oscillate the geometry or reports",
  );
  await page.setViewportSize({ width: 540, height: 800 });
  await page.evaluate(() => window.reportMode("hold"));
  const fresh = await page.evaluate(() =>
    window.changeProgress("Fresh revision."),
  );
  await page.waitForFunction(
    (revision) => reports.at(-1)?.body.revision === revision,
    fresh,
  );
  assert.equal(
    await panel
      .getByRole("button", { name: "Cannot report panel size" })
      .count(),
    0,
    "A new revision must not show the old report error",
  );
  await page.evaluate(() => window.visibility(true));
  await page.waitForFunction(() => reports.at(-1)?.aborted === true);
  const hidden = await page.evaluate(() => reports.length);
  await page.waitForTimeout(1100);
  assert.equal(await page.evaluate(() => reports.length), hidden);
  await page.evaluate(() => {
    window.reportMode("stale");
    window.visibility(false);
  });
  await page.waitForFunction((count) => reports.length > count, hidden);
  assert.equal(
    await panel
      .getByRole("button", { name: "Cannot report panel size" })
      .count(),
    0,
    "Stale 409 is handled by the next poll",
  );
  await page.evaluate(() => window.reportMode("hold"));
  await change("Held report.");
  const held = await page.evaluate(() => reports.length);
  await page.waitForTimeout(1100);
  assert.equal(await page.evaluate(() => reports.length), held);
  await page.evaluate(() => {
    window.online(false);
  });
  await page.waitForFunction(() => reports.at(-1)?.aborted);
  await page.evaluate(() => {
    window.reportMode("ok");
    window.online(true);
    window.switchAgent("second");
  });
  await page.waitForFunction(() => reports.at(-1)?.body.agent === "second");
  assert.equal(await panel.getAttribute("data-agent"), "second");
  assert.equal(await page.evaluate(() => maxActiveReports), 1);
  const payloads = await page.evaluate(() => reports.map((item) => item.body));
  execFileSync(
    "/opt/homebrew/bin/python3",
    [
      "-B",
      "-c",
      'import json,sys;sys.path.insert(0,"scripts");from codex_progress_layout import _report;[_report(row) for row in json.load(sys.stdin)];print("Browser report payloads match backend validator")',
    ],
    { cwd: join(root, ".."), input: JSON.stringify(payloads) },
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS ${engine.name()}: exact fit, wrapped markers, resize/height/font invalidation, unsupported rejection, source link, full file access, report identity/heartbeat/failure/cancel/409, no overlapping reports`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
