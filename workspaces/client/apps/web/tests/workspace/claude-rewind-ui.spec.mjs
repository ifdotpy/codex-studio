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
    server: { host: "127.0.0.1", port: 0, watch: null, hmr: false },
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
          const agent={id:'worker',provider:'claude',workspaceMode:"shared",worktreeReady:false,status:'paused'};
          function Fixture(){const [revision,setRevision]=useState(0);return <MantineProvider><Restore agent={agent} revision={revision} onRestored={()=>setRevision(v=>v+1)}/></MantineProvider>;}
          createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  const writes = [];
  const errors = [];
  const releases = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.route("**/api/claude/file-backups", async (route) => {
      const body = route.request().postDataJSON();
      if (body.release) releases.push(body);
      return route.fulfill({
        json: {
          count: body.release ? 99 : 100,
          bytes: 1000,
          maxCount: 100,
          maxBytes: 1073741824,
          released: body.release ? 1 : 0,
        },
      });
    });
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
              status: 400,
              json: {
                error:
                  "The before-restore backup limit is reached. Release completed backups before another restore.",
              },
            }
          : writes.length === 2
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
    await expect(page.getByRole("alert")).toContainText("backup limit");
    await expect(
      page.getByText("Backup usage: 100 of 100 copies", { exact: false }),
    ).toBeVisible();
    await page.screenshot({
      path: testInfo.outputPath("claude-backup-quota-320.png"),
      fullPage: true,
    });
    await page
      .getByRole("button", { name: "Release completed backups" })
      .click();
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(releases).toHaveLength(1);
    await button.click();
    await expect(page.getByRole("alert")).toContainText(
      "The restore reply was lost.",
    );
    await expect(button).toBeEnabled();
    await button.click();
    await expect(select).toHaveValue("");
    expect(writes).toHaveLength(3);
    expect(writes[2]).toEqual(writes[0]);
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
