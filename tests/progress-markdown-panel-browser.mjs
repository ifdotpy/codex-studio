import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const cacheDir = await mkdtemp(join(tmpdir(), "studio-progress-panel-"));
const entry = join(root, "progress-panel-fixture.tsx");
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
      name: "progress-panel-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/progress-panel-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/progress-panel-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {flushSync} from 'react-dom';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import AgentPanel from '/src/components/AgentPanel.tsx';import '/src/studio-theme.css';
function Fixture(){const[agent,setAgent]=useState('first'),[scope,setScope]=useState('/workspace-a'),[draft,setDraft]=useState('Saved draft');window.switchAgent=id=>flushSync(()=>setAgent(id));window.switchScope=id=>flushSync(()=>setScope(id));return <MantineProvider><div style={{width:360,height:500,display:'flex',flexDirection:'column'}}><div id='history' style={{height:150,overflow:'auto',flex:'0 0 auto'}}><div style={{height:1000}}>Saved conversation</div></div><AgentPanel agentId={agent} stateDir={scope}/><textarea id='composer' value={draft} onChange={event=>setDraft(event.target.value)}/></div></MantineProvider>}createRoot(document.getElementById('root')).render(<Fixture/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({ viewport: { width: 390, height: 700 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    let hidden = false;
    Object.defineProperty(document, "hidden", {
      get: () => hidden,
      configurable: true,
    });
    window.visibility = (value) => {
      hidden = value;
      document.dispatchEvent(new Event("visibilitychange"));
    };
    const original = window.fetch;
    let response = {
      format: "markdown",
      markdown: "Initial **progress**.",
      revision: "1",
      path: "/workspace-a/progress/first/PROGRESS.md",
      error: null,
    };
    let mode = "ok",
      status = 200,
      sequence = 0;
    const requests = [],
      pending = [];
    window.panelRequests = requests;
    window.panelResponse = (next, code = 200) => {
      response = next;
      status = code;
      mode = "ok";
    };
    window.panelMode = (value) => (mode = value);
    window.resolvePanel = () => {
      const call = pending.shift();
      if (call)
        call.resolve(
          new Response(
            JSON.stringify({ ...call.response, agent: call.agent }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
        );
    };
    window.fetch = (input, options) => {
      const url = new URL(String(input), location.href);
      if (url.pathname === "/api/panel/layout")
        return Promise.resolve(new Response("{}", { status: 200 }));
      if (url.pathname !== "/api/panel") return original(input, options);
      const call = {
        agent: url.searchParams.get("agent"),
        id: ++sequence,
        mode,
        aborted: false,
        settled: false,
      };
      requests.push(call);
      options?.signal?.addEventListener("abort", () => {
        call.aborted = true;
      });
      if (mode === "delay")
        return new Promise((resolve) =>
          pending.push({ ...call, response, resolve }),
        );
      if (mode === "hang")
        return new Promise((_resolve, reject) =>
          options.signal.addEventListener("abort", () => {
            call.settled = true;
            reject(new DOMException("Aborted", "AbortError"));
          }),
        );
      call.settled = true;
      if (mode === "offline")
        return Promise.reject(new TypeError("Connection interrupted"));
      return Promise.resolve(
        new Response(JSON.stringify({ ...response, agent: call.agent }), {
          status,
          headers: { "Content-Type": "application/json" },
        }),
      );
    };
  });
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/check`,
    { waitUntil: "commit" },
  );
  const panel = page.getByRole("region", { name: "Agent progress" });
  const current = panel.locator(".agent-panel-current");
  await current.getByText("Initial", { exact: false }).waitFor();
  await page.locator("#history").evaluate((el) => (el.scrollTop = 321));
  await page.locator("#composer").fill("Keep my draft and focus");
  const next = (markdown, revision = "next", error = null) => ({
    format: "markdown",
    markdown,
    revision,
    path: "/fixture/PROGRESS.md",
    error,
  });
  await page.evaluate(
    (value) => window.panelResponse(value),
    next("Updated progress."),
  );
  await current.getByText("Updated progress.", { exact: true }).waitFor();
  assert.equal(
    await page.locator("#composer").inputValue(),
    "Keep my draft and focus",
  );
  assert.equal(
    await page
      .locator("#composer")
      .evaluate((el) => el === document.activeElement),
    true,
  );
  assert.equal(
    await page.locator("#history").evaluate((el) => el.scrollTop),
    321,
  );
  const beforeHidden = await page.evaluate(() => {
    window.visibility(true);
    return panelRequests.length;
  });
  await page.waitForTimeout(1200);
  assert.equal(await page.evaluate(() => panelRequests.length), beforeHidden);
  await page.evaluate(
    (value) => window.panelResponse(value),
    next("Resumed progress."),
  );
  await page.evaluate(() => window.visibility(false));
  await current
    .getByText("Resumed progress.", { exact: true })
    .waitFor({ timeout: 900 });
  const readError = async (message) => {
    await panel
      .getByText("Cannot read PROGRESS.md.", { exact: true })
      .waitFor();
    assert.equal(
      await current.count(),
      0,
      "An old file must not appear as the current revision after a read error",
    );
    await panel
      .getByRole("button", { name: "Error details", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "Progress error" });
    await dialog.getByText(message, { exact: true }).waitFor();
    return dialog;
  };
  const closeError = async (dialog) => {
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
  };
  await page.evaluate(() => window.panelMode("offline"));
  await closeError(await readError("Connection interrupted"));
  const diagnostic = {
    message: "Read denied",
    code: "EACCES",
    path: "/fixture/PROGRESS.md",
  };
  await page.evaluate(
    (value) => window.panelResponse(value, 503),
    next("", null, diagnostic),
  );
  // Wait for the next failed poll before opening its full diagnostic.
  await page.waitForTimeout(1100);
  const dialog = await readError("Read denied");
  await dialog
    .getByRole("button", { name: "Error details", exact: true })
    .click();
  const details = JSON.parse(
    await dialog
      .getByRole("button", { name: "Hide error details", exact: true })
      .locator("..")
      .locator(":scope > span")
      .last()
      .innerText(),
  );
  assert.equal(details.status, 503);
  assert.deepEqual(details.details.error, diagnostic);
  await closeError(dialog);
  await page.evaluate(
    (value) => window.panelResponse(value),
    next("", null, "UTF-8 read failed"),
  );
  await page.waitForTimeout(1100);
  await closeError(await readError("UTF-8 read failed"));
  await page.evaluate(
    (value) => window.panelResponse(value),
    next("Old agent content.", "old"),
  );
  await current.getByText("Old agent content.", { exact: true }).waitFor();
  await page.evaluate(() => window.panelMode("delay"));
  await page.waitForFunction(() => panelRequests.at(-1)?.mode === "delay");
  await page.evaluate(
    (value) => {
      window.panelResponse(value);
      window.switchAgent("second");
    },
    next("Second agent content.", "second"),
  );
  await current.getByText("Second agent content.", { exact: true }).waitFor();
  await page.evaluate(() => window.resolvePanel());
  await page.waitForTimeout(50);
  assert.equal(
    await current.getByText("Old agent content.", { exact: true }).count(),
    0,
  );
  assert.equal(
    await current.getByText("Second agent content.", { exact: true }).count(),
    1,
  );
  await page.evaluate(() => window.panelMode("delay"));
  await page.waitForFunction(() => panelRequests.at(-1)?.mode === "delay");
  await page.evaluate(
    (value) => {
      window.panelResponse(value);
      window.switchScope("/workspace-b");
    },
    next("Other workspace content.", "workspace"),
  );
  await current
    .getByText("Other workspace content.", { exact: true })
    .waitFor();
  await page.evaluate(() => window.resolvePanel());
  await page.waitForTimeout(50);
  assert.equal(
    await current.getByText("Second agent content.", { exact: true }).count(),
    0,
  );
  await page.evaluate(() => window.panelMode("hang"));
  await page.waitForFunction(() => panelRequests.at(-1)?.mode === "hang");
  const hanging = await page.evaluate(() => panelRequests.length);
  await page.waitForTimeout(1200);
  assert.equal(
    await page.evaluate(() => panelRequests.length),
    hanging,
    "One request remains in flight",
  );
  await panel
    .getByText("Cannot read PROGRESS.md.", { exact: true })
    .waitFor({ timeout: 9000 });
  await closeError(await readError("The server did not respond in time."));
  await page.evaluate((value) => window.panelResponse(value), next(""));
  await panel.waitFor({ state: "hidden" });
  await page.evaluate((value) => window.panelResponse(value), {
    ...next(""),
    exists: false,
  });
  await page.waitForTimeout(1100);
  assert.equal(await panel.count(), 0);
  const long =
    "Long progress.\n\n" +
    Array.from({ length: 30 }, (_, i) => `- Step ${i}`).join("\n");
  await page.evaluate((value) => window.panelResponse(value), next(long));
  await panel.getByText("Progress does not fit.", { exact: true }).waitFor();
  assert.equal(await current.count(), 0);
  const geometry = await panel.evaluate((el) => {
    const content = el.querySelector(".agent-panel-content");
    return {
      height: el.getBoundingClientRect().height,
      scroll: content.scrollHeight > content.clientHeight,
      overflow: getComputedStyle(content).overflowY,
    };
  });
  assert.ok(geometry.height <= 150.5);
  assert.equal(geometry.scroll, false);
  assert.notEqual(geometry.overflow, "auto");
  await page.evaluate(
    (value) => window.panelResponse(value),
    next(
      'Safe <img src="bad" onerror="window.executed=true"> [unsafe](javascript:alert(1))',
    ),
  );
  await panel
    .getByText("Progress format is unsupported.", { exact: true })
    .waitFor();
  assert.equal(await current.count(), 0);
  assert.equal(
    await panel.locator('[onerror],a[href^="javascript:"],iframe,img').count(),
    0,
  );
  await page.evaluate((value) => window.panelResponse(value), {
    format: "json-render",
    html: "<button>Old callback</button>",
    css: "",
    version: 9,
    agent: "second",
  });
  await closeError(await readError("The PROGRESS.md response is invalid."));
  assert.equal(
    await panel.getByText("Old callback", { exact: true }).count(),
    0,
  );
  assert.equal(
    await page.locator("#composer").inputValue(),
    "Keep my draft and focus",
  );
  assert.equal(
    await page.locator("#history").evaluate((el) => el.scrollTop),
    321,
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS ${browserType.name()}: file polling, hide/resume, deadlines, scoped late replies, explicit read errors, empty/removal, rejected unsupported/overflow content, bounded height and unchanged composer/history`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
