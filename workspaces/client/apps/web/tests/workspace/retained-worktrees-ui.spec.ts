import { createRequire } from "node:module";
const { test, expect } = createRequire(import.meta.url)("@playwright/test");
import { createServer } from "vite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("retained folders of live and deleted workers show at both widths", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "retained-worktrees-ui-"));
  const entry = join(root, "retained-worktrees-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, watch: null, hmr: false },
    plugins: [
      {
        name: "retained-worktrees-fixture",
        enforce: "pre",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/retained-worktrees-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/retained-worktrees-fixture.tsx") return entry;
          if (/\/servers\/ServerServicesContext(?:\.tsx)?$/.test(id))
            return "\0retained-api";
        },
        load(id) {
          if (id === "\0retained-api")
            return `
          const get=async(path,options)=>{const r=await fetch(path+'?'+new URLSearchParams(options.query));return r.json();};
          const api={get};
          export const useServerApi=()=>api;
          const watch=(_resource,read,failed)=>{let active=true;window.refreshRetained=async()=>{if(active)await read().catch(failed);};queueMicrotask(()=>{if(active)void window.refreshRetained();});return()=>{active=false;};};
          export const useServerResourceReads=()=>watch;
          export const useServerFileDialogs=()=>({canRevealPath:()=>true,revealPath:async(path)=>{window.openedFolder=path;}});`;
          if (id === entry)
            return `
          import React from 'react';import {createRoot} from 'react-dom/client';
          import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';
          import {RetainedWorktree,TeamRetainedWorktrees,useRetainedWorktrees} from '/src/components/agents/RetainedWorktrees.tsx';
          const worker={id:'worker',worktree:true,rootId:'lead',status:'completed',workspaceCleanupIntent:{phase:'not_sent',reason:'Submodule vendor/documentation has local changes. Cleanup kept the worktree.',path:'/Users/example/Projects/studio/.worktrees/codex-agents/worker/vendor/documentation'}};
          const lead={id:'lead',isLead:true};
          function Team(){const items=useRetainedWorktrees(lead,[worker]);return <><TeamRetainedWorktrees items={items}/><RetainedWorktree agent={worker}/></>;}
          createRoot(document.getElementById('root')).render(<MantineProvider><main style={{maxWidth:900,margin:'auto',padding:12,display:'grid',gap:12}}><h1>Team</h1><Team/></main></MantineProvider>);`;
        },
      },
    ],
  });
  const errors = [];
  let cleanupComplete = false;
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.route("**/api/retained-worktrees?*", (route) =>
      route.fulfill({
        json: {
          rootId: "lead",
          retainedWorktrees: [
            // A live worker shows on its own card, not twice.
            {
              id: "worker",
              name: "Live worker",
              cleanup: { phase: "not_sent", reason: null, path: "/work/live" },
            },
            ...(cleanupComplete
              ? [
                  {
                    id: "deleted-worker",
                    name: "Deleted worker",
                    cleanup: {
                      phase: "unknown",
                      reason: null,
                      path: "/Users/example/Projects/studio/.worktrees/codex-agents/deleted-worker",
                    },
                  },
                ]
              : []),
          ],
        },
      }),
    );
    await page.setViewportSize({ width: 320, height: 900 });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await expect(
      page.getByText("Worktree retained", { exact: true }),
    ).toHaveCount(1);
    // Cleanup finishes after the deleted worker is absent from the live team.
    await expect(page.getByText("Deleted worker", { exact: true })).toHaveCount(
      0,
    );
    cleanupComplete = true;
    await page.evaluate(() => window.refreshRetained());
    for (const text of ["Deleted worker", "Cleanup outcome unconfirmed"])
      await expect(page.getByText(text, { exact: true })).toBeVisible();
    await expect(page.getByText("Live worker", { exact: true })).toHaveCount(0);
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 1100 });
      await page.screenshot({
        path: testInfo.outputPath(`retained-worktrees-${width}.png`),
        fullPage: true,
      });
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth,
        ),
      ).toBe(true);
    }
    await page
      .getByRole("button", { name: "Open folder", exact: true })
      .first()
      .click();
    expect(await page.evaluate(() => window.openedFolder)).toBe(
      "/Users/example/Projects/studio/.worktrees/codex-agents/deleted-worker",
    );
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
