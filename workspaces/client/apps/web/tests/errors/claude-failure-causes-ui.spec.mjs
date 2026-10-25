import { apiSchemaHeaders, test, expect } from "../playwright.mjs";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// Real failed-turn and notice components. The metadata follows the bridge contract.
test("Claude failure causes at 320 and 1200 pixels", async ({
  page,
}, testInfo) => {
  test.setTimeout(120000);
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const scratch = await mkdtemp(join(tmpdir(), "claude-failure-ui-"));
  const harness = `
    import React from 'react';
    import {createRoot} from 'react-dom/client';
    import {MantineProvider} from '@mantine/core';
    import '@mantine/core/styles.css';
    import '/src/style.css';
    import TurnHistory from '/src/components/conversation/transcript/TurnHistory.tsx';
    import {NativeNotice} from '/src/components/NativeNotice.tsx';
    const agent={id:'agent',threadId:'thread',provider:'claude',status:'failed',epoch:1};
    const failure={message:'Provider failed',codexErrorInfo:'badRequest',claudeFailure:{apiError:'tool_history_mismatch',apiErrorStatus:400,apiErrorCode:'tool_pair_gate'}};
    const items=[{id:'input',role:'user',text:'Update the report.',turnId:'turn',turnStatus:'failed',turnError:failure}];
    const notice={id:'notice',role:'system',text:'Provider failed',nativeNotice:'error',nativeError:failure};
    const tokenNotice={...notice,id:'tokens',nativeError:{claudeFailure:{apiErrorCode:'context_gate',promptTokens:210001,maxContextTokens:200000}}};
    const credentials={...notice,id:'credentials',nativeError:{claudeFailure:{apiError:'provider_credentials',provider:'bedrock',remedy:'refresh_credentials'}}};
    createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,null,
      React.createElement('main',{style:{maxWidth:900,margin:'auto',padding:12}},
        React.createElement('h1',null,'Claude failure causes'),
        React.createElement('h2',null,'Failed turn'),
        React.createElement(TurnHistory,{items,agent,agentId:'agent',enabled:true,storageKey:'failure-fixture',onJump:()=>{},renderMessage:item=>item.nativeNotice?React.createElement(NativeNotice,{item}):React.createElement('p',null,item.text)}),
        React.createElement('h2',null,'Native notice'),
        React.createElement(NativeNotice,{item:notice}),
        React.createElement('h2',null,'Context limit'),
        React.createElement(NativeNotice,{item:tokenNotice}),
        React.createElement('h2',null,'Account access'),
        React.createElement(NativeNotice,{item:credentials})
      )));
  `;
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(scratch, "vite"),
    plugins: [
      {
        name: "claude-failure-fixture",
        resolveId(id) {
          if (id === "virtual:claude-failure") return "\0" + id;
        },
        load(id) {
          if (id === "\0virtual:claude-failure") return harness;
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  const errors = [],
    writes = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module">import "/@id/__x00__virtual:claude-failure";</script>',
    }),
  );
  await page.route("**/api/**", (route) => {
    if (route.request().method() !== "GET") writes.push(route.request().url());
    return route.fulfill({
      headers: apiSchemaHeaders(),
      json: { items: [], turns: [], requests: [] },
    });
  });
  await server.listen();
  try {
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 1100 });
      await page.goto(server.resolvedUrls.local[0] + "check");
      await expect(page.locator('[data-outcome="failed"]')).toContainText(
        "Tool history does not match (tool_pair_gate)",
      );
      const notice = page.locator('[data-message="notice"]');
      await expect(notice).toContainText("Tool history does not match");
      await expect(notice).toContainText("tool calls and results do not match");
      await notice.locator("summary").click();
      await expect(notice.locator("pre")).toContainText(
        '"apiErrorCode": "tool_pair_gate"',
      );
      await expect(page.locator('[data-message="tokens"]')).toContainText(
        "210001 tokens. Limit: 200000 tokens.",
      );
      await expect(page.locator('[data-message="credentials"]')).toContainText(
        "Provider credentials failed",
      );
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      const screenshot = testInfo.outputPath(`claude-failure-${width}.png`);
      await page.screenshot({ path: screenshot, fullPage: true });
      await testInfo.attach(`claude-failure-${width}`, {
        path: screenshot,
        contentType: "image/png",
      });
    }
    expect(errors).toEqual([]);
    expect(writes).toEqual([]);
  } finally {
    await server.close();
    await rm(scratch, { recursive: true, force: true });
  }
});
