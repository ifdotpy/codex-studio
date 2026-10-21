import { createRequire } from "node:module";
const { test, expect } = createRequire(import.meta.url)("@playwright/test");
import { createServer } from "vite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("Claude checkpoints show the reason and send one combined restore identity", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "claude-rewind-ui-"));
  const entry = join(root, "rewind-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "rewind-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/rewind-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/rewind-fixture.tsx") return entry;
          if (id.endsWith("/servers/ServerServicesContext"))
            return "\0rewind-api";
        },
        load(id) {
          if (id === "\0rewind-api")
            return `
          const post = async (path,body) => {const response=await fetch(path,{method:'POST',body:JSON.stringify(body)});const result=await response.json();if(!response.ok)throw new Error(result.error);return result;};
          const api={post,saved:(key,fallback)=>JSON.parse(localStorage.getItem(key)||'null')||fallback};
          export const useServerApi=()=>api;
          export const useServerStorage=()=>localStorage;`;
          if (id === entry)
            return `
          import React,{useState} from 'react';
          import {createRoot} from 'react-dom/client';
          import {MantineProvider} from '@mantine/core';
          import '@mantine/core/styles.css';
          import Restore from '/src/components/ClaudeCheckpointRestore.tsx';
          const agent={id:'worker',provider:'claude',imageWorkspace:true,worktreeReady:false,status:'paused'};
          function Fixture(){const [revision,setRevision]=useState(0);return <MantineProvider><Restore agent={agent} revision={revision} onRestored={()=>setRevision(v=>v+1)}/></MantineProvider>;}
          createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  const writes = [];
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.route("**/api/claude/session", async (route) => {
      const body = route.request().postDataJSON();
      if (body.action === "state")
        return route.fulfill({
          json: {
            turns: [
              {
                id: "old",
                status: "completed",
                text: "Old message",
                canRestoreFiles: false,
                restoreFilesUnavailableReason:
                  "This message has no SDK file checkpoint.",
              },
              {
                id: "ready",
                status: "completed",
                text: "Edit the source file",
                canRestoreFiles: true,
                restoreFilesUnavailableReason: null,
              },
            ],
          },
        });
      writes.push(body);
      return route.fulfill(
        writes.length === 1
          ? {
              status: 409,
              json: {
                error: "The restore reply was lost. Retry the same request.",
              },
            }
          : {
              json: {
                threadId: "thread",
                nativeId: "rewound",
                filesRestored: true,
              },
            },
      );
    });
    await page.setViewportSize({ width: 320, height: 800 });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const select = page.getByLabel("Message to restore before");
    const button = page.getByRole("button", {
      name: "Restore files and conversation",
    });
    await select.selectOption("old");
    await expect(button).toBeDisabled();
    await expect(
      page.getByText("This message has no SDK file checkpoint."),
    ).toBeVisible();
    await select.selectOption("ready");
    await expect(button).toBeEnabled();
    await page.screenshot({
      path: testInfo.outputPath("claude-rewind-320.png"),
      fullPage: true,
    });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await button.click();
    await expect(page.getByRole("alert")).toContainText(
      "The restore reply was lost.",
    );
    await expect(button).toBeEnabled();
    await button.click();
    await expect(select).toHaveValue("");
    expect(writes).toHaveLength(2);
    expect(writes[1]).toEqual(writes[0]);
    expect(writes[0]).toMatchObject({
      id: "worker",
      action: "rollback",
      turn_id: "ready",
      restore_files: true,
    });
    expect(writes[0].request_id).toBeTruthy();
    await page.setViewportSize({ width: 1200, height: 800 });
    await select.selectOption("ready");
    await expect(button).toBeEnabled();
    await page.screenshot({
      path: testInfo.outputPath("claude-rewind-1200.png"),
      fullPage: true,
    });
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
