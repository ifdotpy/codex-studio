import playwright from "@playwright/test";
import { createServer } from "vite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const { test, expect } = playwright;
test.describe.configure({ mode: "parallel" });

// A headless fixture mounts the production Workspace Tools caller. The API
// fixture controls delayed replies, response loss and inventory pagination.
for (const width of [320, 1200]) {
  test(`MCP reconnect in Workspace at ${width}px`, async ({
    page,
  }, testInfo) => {
    const root = resolve(import.meta.dirname, "../..");
    const cacheDir = await mkdtemp(join(tmpdir(), "mcp-reconnect-ui-"));
    const entry = join(root, "mcp-reconnect-fixture.tsx");
    const server = await createServer({
      configFile: false,
      root,
      cacheDir,
      server: { host: "127.0.0.1", port: 0, hmr: false },
      plugins: [
        {
          name: "mcp-reconnect-fixture",
          configureServer(server) {
            server.middlewares.use("/check", (_req, res) => {
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/mcp-reconnect-fixture.tsx"></script>',
              );
            });
          },
          resolveId(id) {
            if (id === "/mcp-reconnect-fixture.tsx") return entry;
          },
          load(id) {
            if (id !== entry) return;
            return `import React from 'react';
            import {createRoot} from 'react-dom/client';
            import {MantineProvider} from '@mantine/core';
            import '@mantine/core/styles.css';
            import {Workspace} from '/src/components/shell/Workspace.tsx';
            import {ServerServicesProvider} from '/src/servers/ServerServicesContext.tsx';
            const agent={id:'agent',name:'Claude fixture',source:'managed',provider:'claude',isLead:true};
            const api={get:async(path,options)=>{const r=await fetch(path+'?'+new URLSearchParams(options?.query));return r.json()},post:async(path,body)=>{const r=await fetch(path,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});const v=await r.json();if(!r.ok)throw new Error(v.error);return v}};
            const services={owner:{id:'fixture',origin:location.origin},api,watchResourceReads:()=>()=>{}};
            const data={stateDir:'/fixture',threads:[agent],runtime:{agents:[agent],requests:[],complaints:[],messages:[],rooms:[],monitors:[],tasks:[],work:[],rules:[]}};
            createRoot(document.getElementById('root')).render(<MantineProvider><ServerServicesProvider services={services}><Workspace initialSection="tools" opened={true} onClose={()=>{}} agent={agent} data={data} allRequests={[]} onSelect={()=>{}} refresh={async()=>{}} notify={()=>{}}/></ServerServicesProvider></MantineProvider>);`;
          },
        },
      ],
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const calls = [];
    const pageReads = [];
    let connected = false;
    let thread = "thread";
    let release;
    const delayed = new Promise((resolve) => {
      release = resolve;
    });
    let mode = "delayed";
    const inventory = (cursor) => ({
      agent: "agent",
      at: 1,
      model: "model",
      role: "orchestrator",
      managed: [],
      observed: [],
      observedNative: [],
      nativeInventory: "No recorded calls.",
      skills: [],
      servers: [],
      errors: [],
      mcpSession: {
        account: "account",
        thread,
        epoch: 2,
        connection: "connection",
        generation: 1,
      },
      mcp: cursor
        ? [{ name: "second-server", status: "connected" }]
        : [
            {
              name: "server-with-a-long-name-for-small-window-layout-check",
              status: connected ? "connected" : "failed",
              error: connected
                ? undefined
                : "The MCP endpoint refused the connection. Reconnect this server to try again.",
            },
          ],
      serversCursor: cursor ? null : "next",
    });
    try {
      await server.listen();
      await page.setViewportSize({ width, height: 1000 });
      await page.route("**/api/capabilities?*", (route) => {
        const cursor = new URL(route.request().url()).searchParams.get(
          "serversCursor",
        );
        pageReads.push(cursor);
        return route.fulfill({ json: inventory(cursor) });
      });
      await page.route("**/api/mcp/reconnect", async (route) => {
        calls.push(route.request().postDataJSON());
        if (mode === "delayed") await delayed;
        if (mode === "lost") {
          mode = "replay";
          return route.abort("failed");
        }
        if (mode === "codex")
          return route.fulfill({
            status: 400,
            json: { error: "MCP server reconnect is not supported for Codex." },
          });
        connected = true;
        return route.fulfill({
          json: { status: "completed", message: "Reconnect completed." },
        });
      });
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/check`,
      );
      const first = page.getByRole("region", {
        name: "MCP server server-with-a-long-name-for-small-window-layout-check",
        exact: true,
      });
      await expect(first).toContainText("Status: failed");
      await page.getByRole("button", { name: "Load more servers" }).click();
      await expect(
        page.getByRole("region", {
          name: "MCP server second-server",
          exact: true,
        }),
      ).toBeVisible();
      expect(pageReads).toContain("next");
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(first).toContainText("Reconnect in progress.");
      await expect(first.getByRole("button")).toBeDisabled();
      release();
      await expect(first).toContainText("Reconnect completed.");
      await expect(first).toContainText("Status: connected");
      expect(calls[0].session.thread).toBe("thread");
      expect(calls[0].name).toContain("server-with-a-long-name");
      mode = "lost";
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(
        first.getByRole("button", { name: "Check reconnect result" }),
      ).toBeEnabled();
      await page.reload();
      await first
        .getByRole("button", { name: "Check reconnect result", exact: true })
        .click();
      await expect(first).toContainText("Reconnect completed.");
      expect(calls[2].request_id).toBe(calls[1].request_id);
      mode = "codex";
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(first).toContainText("not supported for Codex");
      await first.scrollIntoViewIfNeeded();
      const box = await first.boundingBox();
      expect(box.x).toBeGreaterThanOrEqual(0);
      expect(box.x + box.width).toBeLessThanOrEqual(width);
      expect(
        await first.evaluate((node) => node.scrollWidth <= node.clientWidth),
      ).toBe(true);
      await page.screenshot({
        path: testInfo.outputPath(`mcp-reconnect-${width}.png`),
        fullPage: true,
      });
      thread = "new-thread";
      await page
        .getByRole("button", { name: "Refresh servers", exact: true })
        .click();
      await expect(
        first.getByRole("button", { name: "Reconnect", exact: true }),
      ).toBeEnabled();
      mode = "replay";
      await first
        .getByRole("button", { name: "Reconnect", exact: true })
        .click();
      await expect(first).toContainText("Reconnect completed.");
      expect(calls.at(-1).session.thread).toBe("new-thread");
      expect(calls.at(-1).request_id).not.toBe(calls.at(-2).request_id);
      expect(errors).toEqual([]);
    } finally {
      release();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}
