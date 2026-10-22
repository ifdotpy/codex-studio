import { createRequire } from "node:module";
const { test, expect } = createRequire(import.meta.url)("@playwright/test");
import { createServer } from "vite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("agent limits show effective capabilities, budget holds, and retained folders at both widths", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "agent-limits-ui-"));
  const entry = join(root, "agent-limits-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, watch: null, hmr: false },
    plugins: [
      {
        name: "agent-limits-fixture",
        enforce: "pre",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/agent-limits-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/agent-limits-fixture.tsx") return entry;
          if (/\/servers\/ServerServicesContext(?:\.tsx)?$/.test(id))
            return "\0limits-api";
        },
        load(id) {
          if (id === "\0limits-api")
            return `
          const post=async(path,body)=>{const r=await fetch(path,{method:'POST',body:JSON.stringify(body)});const v=await r.json();if(!r.ok)throw new Error(v.error);return v;};
          const get=async(path,options)=>{const r=await fetch(path+'?'+new URLSearchParams(options.query));return r.json();};
          const api={post,get,saved:(key,fallback)=>JSON.parse(localStorage.getItem(key)||'null')||fallback};
          export const useServerApi=()=>api;
          export const useServerStorage=()=>localStorage;
          export const useServerFileDialogs=()=>({canRevealPath:()=>true,revealPath:async(path)=>{window.openedFolder=path;}});`;
          if (id === entry)
            return `
          import React from 'react';import {createRoot} from 'react-dom/client';
          import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';
          import {WorkerLimits,RetainedWorktree} from '/src/components/agents/AgentLimits.tsx';
          import TeamBudget from '/src/components/agents/TeamBudget.tsx';
          const worker={id:'worker',worktree:true,rootId:'lead',status:'completed',completedWorkerTurns:2,lastCompletedTurn:'turn-2',terminalReason:'worker_max_turns',budgetBlocked:'Usage accounting incomplete',capabilityProfile:{allowedTools:['Read*','mcp__documentation__lookup'],disallowedTools:['Agent','Bash'],skills:['research'],mcpServers:[],maxTurns:2},workspaceCleanupIntent:{phase:'not_sent',reason:'Submodule vendor/documentation has local changes. Cleanup kept the worktree.',path:'/Users/example/Projects/studio/.worktrees/codex-agents/worker/vendor/documentation'}};
          const lead={id:'lead',isLead:true,tokenBudget:100000,tokensUsed:12000};
          createRoot(document.getElementById('root')).render(<MantineProvider><main style={{maxWidth:900,margin:'auto',padding:12,display:'grid',gap:12}}><h1>Team</h1><TeamBudget lead={lead} members={[worker]}/><WorkerLimits agent={worker}/></main></MantineProvider>);`;
        },
      },
    ],
  });
  const continues = [],
    budgets = [],
    errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.route("**/api/team-budget?*", (route) =>
      route.fulfill({
        json: {
          rootId: "lead",
          retainedWorktrees: [
            {
              id: "deleted-worker",
              name: "Deleted worker",
              cleanup: {
                phase: "not_sent",
                reason:
                  "Submodule vendor/documentation has local changes. Cleanup kept the worktree.",
                path: "/Users/example/Projects/studio/.worktrees/codex-agents/worker/vendor/documentation",
              },
            },
          ],
          tokenBudget: 100000,
          tokensUsed: 24000,
          reached: false,
          incomplete: [
            {
              agentId: "worker",
              reason: "Native usage history is unavailable",
            },
          ],
        },
      }),
    );
    await page.route("**/api/configure", (route) => {
      budgets.push(route.request().postDataJSON());
      return route.fulfill({ json: { id: "lead", tokenBudget: 150000 } });
    });
    await page.route("**/api/agents/continue", (route) => {
      continues.push(route.request().postDataJSON());
      return route.fulfill(
        continues.length === 1
          ? {
              status: 409,
              json: { error: "The reply was lost. Retry the same request." },
            }
          : { json: { id: continues[0].request_id, status: "queued" } },
      );
    });
    await page.setViewportSize({ width: 320, height: 900 });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.getByText("Effective capabilities", { exact: true }).click();
    for (const text of [
      "Stopped: turn limit reached",
      "Usage accounting incomplete.",
      "Remaining: Unavailable",
      "Worktree retained",
      "No user MCP servers",
    ])
      await expect(page.getByText(text, { exact: true })).toBeVisible();
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 1100 });
      await page.screenshot({
        path: testInfo.outputPath(`agent-limits-${width}.png`),
        fullPage: true,
      });
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth,
        ),
      ).toBe(true);
    }
    await page.getByLabel("Token budget", { exact: true }).fill("150000");
    await page
      .getByRole("button", { name: "Save budget", exact: true })
      .click();
    await expect.poll(() => budgets.length).toBe(1);
    expect(budgets[0]).toEqual({ id: "lead", tokenBudget: 150000 });
    await page
      .getByRole("button", { name: "Open folder", exact: true })
      .click();
    expect(await page.evaluate(() => window.openedFolder)).toBe(
      "/Users/example/Projects/studio/.worktrees/codex-agents/worker/vendor/documentation",
    );
    await page.getByRole("button", { name: "Continue", exact: true }).click();
    await expect(page.getByRole("alert")).toContainText("The reply was lost.");
    await page.getByRole("button", { name: "Continue", exact: true }).click();
    await expect(page.getByText("Continuation queued.")).toBeVisible();
    expect(continues).toHaveLength(2);
    expect(continues[1]).toEqual(continues[0]);
    expect(continues[0].request_id).toBeTruthy();
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
