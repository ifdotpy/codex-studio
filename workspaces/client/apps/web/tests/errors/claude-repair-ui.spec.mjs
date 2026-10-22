import { apiSchemaHeaders, test, expect } from "../playwright.mjs";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// Real chat components with a read-only HTTP fixture. No provider query or desktop input.
test("Claude unconfirmed turn and journal repair at 320 and 1200 pixels", async ({
  page,
}, testInfo) => {
  test.setTimeout(120000);
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const scratch = await mkdtemp(join(tmpdir(), "claude-repair-ui-"));
  const harness = `
    import React from 'react';
    import {createRoot} from 'react-dom/client';
    import {MantineProvider} from '@mantine/core';
    import '@mantine/core/styles.css';
    import TurnHistory from '/src/components/conversation/transcript/TurnHistory.tsx';
    import ClaudeRepairNotice from '/src/components/ClaudeRepairNotice.tsx';
    import {NativeNotice} from '/src/components/NativeNotice.tsx';
    const agent={id:'agent',threadId:'thread',provider:'claude',status:'paused',epoch:1,error:'CLAUDE_SESSION_REPAIR_REQUIRED: needs repair'};
    const items=[{id:'input',role:'user',text:'Update the report.',turnId:'turn',turnStatus:'uncertain'},
      {id:'answer',role:'assistant',text:'I read the source files.',turnId:'turn',turnStatus:'uncertain',turnError:{message:'Studio could not confirm the Claude turn outcome. The native transcript is incomplete.'}}];
    createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,null,
      React.createElement('main',{style:{maxWidth:900,margin:'auto',padding:12}},
        React.createElement('h1',null,'Conversation'),
        React.createElement(ClaudeRepairNotice,{agent}),
        React.createElement(TurnHistory,{items,agent,agentId:'agent',enabled:true,storageKey:'repair-fixture',onJump:()=>{},renderMessage:item=>item.nativeNotice?React.createElement(NativeNotice,{item}):React.createElement('p',null,item.text)})
      )));
  `;
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(scratch, "vite"),
    plugins: [
      {
        name: "claude-repair-fixture",
        resolveId(id) {
          if (id === "virtual:claude-repair") return "\0" + id;
        },
        load(id) {
          if (id === "\0virtual:claude-repair") return harness;
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  const writes = [],
    errors = [];
  let reportStatus = "needs_repair";
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module">import "/@id/__x00__virtual:claude-repair";</script>',
    }),
  );
  await page.route("**/api/**", (route) => {
    const request = route.request();
    if (request.method() !== "GET")
      writes.push(request.method() + " " + request.url());
    const path = new URL(request.url()).pathname;
    return route.fulfill({
      headers: apiSchemaHeaders(),
      json:
        path === "/api/claude/repair-report"
          ? {
              status: reportStatus,
              skippedRows: 3,
              invalidSequence: reportStatus === "needs_repair",
              scopeConfirmed: true,
              nativeHistoryConfirmed: reportStatus === "repaired",
            }
          : { items: [], turns: [], requests: [] },
    });
  });
  await server.listen();
  try {
    for (const width of [320, 1200]) {
      for (const status of ["needs_repair", "repaired"]) {
        reportStatus = status;
        await page.setViewportSize({ width, height: 1000 });
        await page.goto(server.resolvedUrls.local[0] + "check");
        await expect(
          page.getByText("Outcome unconfirmed", { exact: true }),
        ).toBeVisible();
        await expect(
          page.getByText(
            status === "repaired" ? "Journal repaired" : "Journal needs repair",
            { exact: true },
          ),
        ).toBeVisible();
        await expect(
          page.getByText("Skipped rows: 3.", { exact: true }),
        ).toBeVisible();
        await page.getByText("Repair report", { exact: true }).click();
        await expect(
          page.getByText("Missing evidence", { exact: true }),
        ).toBeVisible();
        await expect(
          page.getByText(
            status === "repaired"
              ? "No missing evidence was reported."
              : "A complete journal sequence.",
            { exact: true },
          ),
        ).toBeVisible();
        await expect(page.locator('[data-outcome="uncertain"]')).toBeVisible();
        await expect(
          page.getByRole("button", {
            name: /retry|replay|repair|another answer/i,
          }),
        ).toHaveCount(0);
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
        const screenshot = testInfo.outputPath(
          `claude-repair-${status}-${width}.png`,
        );
        await page.screenshot({ path: screenshot, fullPage: true });
        await testInfo.attach(`claude-repair-${status}-${width}`, {
          path: screenshot,
          contentType: "image/png",
        });
      }
    }
    expect(writes).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(scratch, { recursive: true, force: true });
  }
});
