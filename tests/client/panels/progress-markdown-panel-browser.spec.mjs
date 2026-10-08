import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  readApiSchemaHash,
  test,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Progress markdown panel browser", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(45_000);
  const root = join(import.meta.dirname, "../../../web");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-progress-panel-"));
  const workspaceId = "1234567890abcdef1234567890abcdef";
  const entry = join(root, "progress-panel-fixture.tsx");
  const streams = new Set();
  let resourceRevision = 0;
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
        "@mantine/hooks",
        "lucide-react",
        "marked",
        "dompurify",
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
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
          server.middlewares.use("/api/sync/stream", (req, res) => {
            res.setHeader("Content-Type", "text/event-stream");
            res.write(apiSchemaHandshakeSse());
            res.flushHeaders();
            const resources = JSON.parse(
              new URL(req.url, "http://127.0.0.1").searchParams.get(
                "resources",
              ) || "[]",
            );
            streams.add(res);
            res.on("close", () => streams.delete(res));
            res.write(
              `event: resources\ndata: ${JSON.stringify({
                protocol: 3,
                workspaceId,
                epoch: "fixture",
                revision: ++resourceRevision,
                reason: "initial",
                resources,
                resourceVersions: resources.map((resource) => ({
                  resource,
                  revision: resourceRevision,
                })),
              })}\n\n`,
            );
          });
          server.middlewares.use("/fixture/panel-change", (req, res) => {
            const agentId = new URL(
              req.url,
              "http://127.0.0.1",
            ).searchParams.get("agent");
            if (agentId) {
              const resource = { kind: "panel", agentId };
              const revision = ++resourceRevision;
              for (const stream of streams)
                stream.write(
                  `event: resources\ndata: ${JSON.stringify({
                    protocol: 3,
                    workspaceId,
                    epoch: "fixture",
                    revision,
                    reason: "change",
                    resources: [resource],
                    resourceVersions: [{ resource, revision }],
                  })}\n\n`,
                );
            }
            res.statusCode = 204;
            res.end();
          });
        },
        resolveId(id) {
          if (id === "/progress-panel-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {flushSync} from 'react-dom';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import AgentPanel from '/src/components/agents/AgentPanel.tsx';import '/src/studio-theme.css';
  function Fixture(){const[agent,setAgent]=useState('first'),[scope,setScope]=useState('/workspace-a'),[draft,setDraft]=useState('Saved draft');window.currentAgent=agent;window.switchAgent=id=>flushSync(()=>setAgent(id));window.switchScope=id=>flushSync(()=>setScope(id));return <MantineProvider><div style={{width:360,height:500,display:'flex',flexDirection:'column'}}><div id='history' style={{height:150,overflow:'auto',flex:'0 0 auto'}}><div style={{height:1000}}>Saved conversation</div></div><AgentPanel agentId={agent} stateDir={scope}/><textarea id='composer' value={draft} onChange={event=>setDraft(event.target.value)}/></div></MantineProvider>}createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 390, height: 700 });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.addInitScript(
      ({ schemaHeader, schemaHash, workspaceId }) => {
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
        const signalPanelChange = () =>
          original(
            `/fixture/panel-change?agent=${encodeURIComponent(window.currentAgent)}`,
          );
        window.panelResponse = async (next, code = 200) => {
          response = next;
          status = code;
          mode = "ok";
          await signalPanelChange();
        };
        window.panelMode = async (value) => {
          mode = value;
          await signalPanelChange();
        };
        window.resolvePanel = () => {
          const call = pending.shift();
          if (call)
            call.resolve(
              new Response(
                JSON.stringify({ ...call.response, agent: call.agent }),
                {
                  status: 200,
                  headers: {
                    "Content-Type": "application/json",
                    [schemaHeader]: schemaHash,
                  },
                },
              ),
            );
        };
        window.fetch = async (input, options = {}) => {
          const request = input instanceof Request ? input : null;
          const url = new URL(
            request ? request.url : String(input),
            location.href,
          );
          if (request)
            options = {
              ...options,
              method: request.method,
              signal: request.signal,
              body:
                request.method === "GET"
                  ? undefined
                  : await request.clone().text(),
            };
          if (url.pathname === "/api/panel/layout")
            return Promise.resolve(
              new Response("{}", {
                status: 200,
                headers: {
                  "Content-Type": "application/json",
                  [schemaHeader]: schemaHash,
                },
              }),
            );
          if (url.pathname === "/api/sync/identity")
            return Promise.resolve(
              new Response(JSON.stringify({ workspaceId }), {
                status: 200,
                headers: {
                  "Content-Type": "application/json",
                  [schemaHeader]: schemaHash,
                },
              }),
            );
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
              headers: {
                "Content-Type": "application/json",
                [schemaHeader]: schemaHash,
              },
            }),
          );
        };
      },
      {
        schemaHeader: API_SCHEMA_HASH_HEADER,
        schemaHash: readApiSchemaHash(),
        workspaceId,
      },
    );
    await page.clock.install();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
      { waitUntil: "commit" },
    );
    const panel = page.getByRole("region", { name: "Agent progress" });
    const current = panel.locator(".agent-panel-current");
    try {
      await panel
        .getByRole("button", { name: "Show", exact: true })
        .click({ timeout: 5_000 });
    } catch (error) {
      const diagnostic = await page.evaluate(() => ({
        requests: window.panelRequests,
        online: navigator.onLine,
      }));
      throw new Error(
        `${error.message}; panel diagnostics ${JSON.stringify(diagnostic)}; page errors ${errors.join("; ")}`,
      );
    }
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
    await page.clock.fastForward(1200);
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
    await panel.getByRole("button", { name: "Hide", exact: true }).click();
    await panel
      .getByRole("button", {
        name: "Cannot read PROGRESS.md. Error details",
        exact: true,
      })
      .waitFor();
    assert.equal(await panel.getAttribute("data-hidden"), "yes");
    await panel
      .locator(".agent-panel-compact-saved")
      .getByText("Saved copy", { exact: true })
      .waitFor();
    await panel
      .getByRole("button", {
        name: "Cannot read PROGRESS.md. Error details",
        exact: true,
      })
      .click();
    const compactError = page.getByRole("dialog", { name: "Progress error" });
    await compactError
      .getByText("Connection interrupted", { exact: true })
      .waitFor();
    await page.keyboard.press("Escape");
    await compactError.waitFor({ state: "hidden" });
    await panel.getByRole("button", { name: "Show", exact: true }).click();
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
    await page.clock.fastForward(1100);
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
    await page.clock.fastForward(1100);
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
      .waitFor({ state: "attached" });
    await panel.getByRole("button", { name: "Show", exact: true }).click();
    await page.evaluate(() => window.resolvePanel());
    await page.waitForTimeout(50);
    assert.equal(
      await current.getByText("Second agent content.", { exact: true }).count(),
      0,
    );
    await page.evaluate(() => window.panelMode("hang"));
    await page.waitForFunction(() => panelRequests.at(-1)?.mode === "hang");
    const hanging = await page.evaluate(() => panelRequests.length);
    await page.clock.fastForward(1200);
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
    await page.clock.fastForward(1100);
    assert.equal(await panel.count(), 0);
    const long =
      "Long progress.\n\n" +
      Array.from({ length: 30 }, (_, i) => `- Step ${i}`).join("\n");
    await page.evaluate((value) => window.panelResponse(value), next(long));
    await panel.getByRole("button", { name: "Expand", exact: true }).waitFor();
    assert.equal(await current.count(), 1);
    const geometry = await panel.evaluate((el) => {
      const content = el.querySelector(".agent-panel-content");
      return {
        height: el.getBoundingClientRect().height,
        clipped: content.scrollHeight > content.clientHeight,
        overflow: getComputedStyle(content).overflowY,
        fade: getComputedStyle(content).maskImage,
      };
    });
    assert.ok(geometry.height <= 196.5);
    assert.equal(geometry.clipped, true);
    assert.equal(geometry.overflow, "clip");
    assert.match(geometry.fade, /linear-gradient/);
    await panel.getByRole("button", { name: "Expand", exact: true }).click();
    assert.equal(await panel.getAttribute("data-expanded"), "yes");
    assert.ok((await panel.boundingBox()).height <= 350.5);
    await panel.locator(".agent-panel-content").evaluate((el) => {
      if (getComputedStyle(el).overflowY !== "auto")
        throw Error("Expand must scroll");
      el.scrollTop = el.scrollHeight;
      if (!el.scrollTop) throw Error("The full file must be accessible");
    });
    await panel.getByRole("button", { name: "Collapse", exact: true }).click();
    assert.equal(await panel.getAttribute("data-expanded"), "no");
    assert.equal(
      await panel
        .locator(".agent-panel-content")
        .evaluate((el) => el.scrollTop),
      0,
    );
    await page.evaluate(
      (value) => window.panelResponse(value),
      next(
        'Safe <img src="bad" onerror="window.executed=true"> [unsafe](javascript:alert(1))',
      ),
    );
    await panel
      .getByText("Open PROGRESS.md to view this format.", { exact: true })
      .waitFor();
    assert.equal(await current.count(), 0);
    assert.equal(
      await panel
        .locator('[onerror],a[href^="javascript:"],iframe,img')
        .count(),
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
      `PASS ${testBrowser.browserType().name()}: file polling, hide/resume, deadlines, scoped late replies, explicit read errors, empty/removal, unsupported rejection, clipped overflow and expansion, bounded height and unchanged composer/history`,
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
