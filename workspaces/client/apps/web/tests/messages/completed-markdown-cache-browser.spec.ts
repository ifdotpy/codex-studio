import assert from "node:assert/strict";
import { join } from "node:path";
import { test, expect, createFixtureViteServer } from "../playwright.ts";

test("Completed Markdown Cache Browser", async ({ page }) => {
  const root = join(import.meta.dirname, "../../");
  const entry = join(root, "markdown-cache-fixture.tsx");
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "completed-markdown-cache-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/markdown-cache-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/markdown-cache-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {flushSync} from 'react-dom';
import {MantineProvider} from '@mantine/core';
import StreamingText from '/src/components/conversation/transcript/StreamingText.tsx';
import ConversationResults from '/src/components/conversation/transcript/ConversationResults.tsx';
import {completedMarkdown} from '/src/native/markdownCache.ts';
window.cache=completedMarkdown;
window.compilations=0;
const save=completedMarkdown.set.bind(completedMarkdown);
completedMarkdown.set=(...args)=>{window.compilations++;return save(...args)};
const initial=Array.from({length:2},(_,i)=>({id:'reply-'+i,text:'Reply '+i+'. Completed **text** with [report][ref].\\n\\n[ref]: ./report.md'}));
const other=initial.map(item=>({...item,id:'other-'+item.id,text:'Other chat '+item.text}));
function Harness(){
 const [items,setItems]=useState(initial),[shown,setShown]=useState(true),[agent,setAgent]=useState('first'),[streaming,setStreaming]=useState(false),[results,setResults]=useState(false);
 window.results=()=>flushSync(()=>setResults(true));
 window.chat=otherChat=>flushSync(()=>setItems(otherChat?other:initial));
 window.hide=()=>flushSync(()=>setShown(false));
 window.show=()=>flushSync(()=>setShown(true));
 window.agent=id=>flushSync(()=>setAgent(id));
 window.text=(text,stream=false)=>flushSync(()=>{setItems([{id:'only',role:'assistant',text}]);setStreaming(stream)});
 return <MantineProvider><>{shown&&items.map(item=><article key={item.id}><StreamingText text={item.text} streaming={streaming} agentId={agent}/></article>)}</>{results&&<ConversationResults messages={[{id:'patch',role:'output',toolStatus:'completed',text:JSON.stringify({type:'fileChange',status:'completed',changes:[{path:'file.txt',diff:'@@ -1 +1 @@\\n-old\\n+saved'}]})}]} agentId={agent}/>}</MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Harness/>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    const errors = [],
      requests = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      console.error("Browser error:", error.message);
    });
    await page.route("**/api/**", (route) => {
      const url = new URL(route.request().url());
      requests.push({
        path: url.pathname,
        agent: url.searchParams.get("agent"),
        method: route.request().method(),
      });
      return route.fulfill({
        json: {
          name: "report.md",
          mime: "text/plain",
          base64: Buffer.from("Current workspace file.").toString("base64"),
        },
      });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
      { waitUntil: "commit" },
    );
    await expect(page.locator("article a")).toHaveCount(2);
    assert.equal(await page.evaluate(() => window.compilations), 2);
    await page.evaluate(() => window.chat(true));
    await expect(page.locator("article").first()).toContainText("Other chat");
    await expect.poll(() => page.evaluate(() => window.compilations)).toBe(4);
    await page.evaluate(() => {
      window.hide();
      window.chat(false);
      window.compilations = 0;
      window.show();
    });
    await expect(page.locator("article a")).toHaveCount(2);
    assert.equal(
      await page.evaluate(() => window.compilations),
      0,
      "Completed exact text survives component remount without parsing again",
    );
    await page.evaluate(() =>
      window.text("[Changed report][r].\n\n[r]: ./changed.md"),
    );
    await expect(
      page.getByRole("link", { name: "Changed report" }),
    ).toBeVisible();
    assert.equal(await page.evaluate(() => window.compilations), 1);
    await page.evaluate(() => {
      window.hide();
      window.agent("second");
      window.show();
    });
    await page.getByRole("link", { name: "Changed report" }).click();
    await page.getByText("Current workspace file.", { exact: true }).waitFor();
    assert.equal(
      requests.at(-1).agent,
      "second",
      "Cached links use the current agent",
    );
    await page.keyboard.press("Escape");
    await page.evaluate(() => window.text("[Remote](file://remote/report.md)"));
    for (let attempt = 0; attempt < 2; attempt++) {
      const trigger = page.getByRole("link", { name: "Remote" });
      await trigger.focus();
      await trigger.press("Enter");
      await page.getByRole("dialog", { name: "Cannot open file" }).waitFor();
      await page
        .getByText("Remote file links are not supported.", { exact: true })
        .waitFor();
      await page.keyboard.press("Escape");
      await page.getByRole("dialog").waitFor({ state: "hidden" });
      await page.waitForFunction(
        () => document.activeElement?.textContent === "Remote",
        null,
        { timeout: 3000 },
      );
    }
    await page.evaluate(() => window.results());
    for (let attempt = 0; attempt < 2; attempt++) {
      const trigger = page.getByRole("button", { name: "file.txt Patch" });
      await trigger.focus();
      await trigger.press("Enter");
      await page
        .getByRole("dialog", { name: "file.txt", exact: true })
        .waitFor();
      await page.keyboard.press("Escape");
      await page.getByRole("dialog").waitFor({ state: "hidden" });
      await page.waitForFunction(
        () => document.activeElement?.textContent === "file.txtPatch",
        null,
        { timeout: 3000 },
      );
    }
    const streamingText = "Streaming-only **prefix**.";
    await page.evaluate((text) => window.text(text, true), streamingText);
    await expect(page.locator("article strong")).toHaveText("prefix");
    await page.evaluate(() => {
      window.hide();
      window.show();
    });
    await expect(page.locator("article strong")).toHaveText("prefix");
    assert.equal(
      await page.evaluate((text) => window.cache.get(text), streamingText),
      undefined,
      "Streaming-only text is never shared in the completed cache",
    );
    await page.evaluate(() =>
      window.text(
        'Safe <img src="bad" onerror="window.executed=true"> [unsafe](javascript:alert(1))',
      ),
    );
    await expect(page.locator("article .markdown-block p")).toContainText(
      "Safe",
    );
    assert.equal(
      await page
        .locator('.prose [onerror],.prose a[href^="javascript:"]')
        .count(),
      0,
    );
    assert.deepEqual(errors, []);
    assert.ok(requests.every((request) => request.method === "GET"));
    console.log(
      "PASS real WASM Markdown cache, exact text, current workspace, dialog focus, sanitizer and streaming exclusion",
    );
  } finally {
    await server.close();
  }
});
