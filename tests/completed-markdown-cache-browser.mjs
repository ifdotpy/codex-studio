import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const baseline = process.env.BASELINE === "1";
const cacheDir = await mkdtemp(join(tmpdir(), "studio-markdown-cache-"));
const entry = join(root, "markdown-cache-fixture.tsx");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
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
        if (
          baseline &&
          ["StreamingText", "ConversationResults"].some(
            (name) => id === join(root, `src/components/${name}.tsx`),
          )
        )
          return execFileSync(
            "git",
            ["show", `dabd21c:web/${id.slice(root.length + 1)}`],
            { cwd: root, encoding: "utf8" },
          );
        if (id !== entry) return;
        return `import React, {useState, Profiler} from 'react';
import {createRoot} from 'react-dom/client';
import {flushSync} from 'react-dom';
import {MantineProvider} from '@mantine/core';
import {marked} from 'marked';
import DOMPurify from 'dompurify';
import StreamingText from '/src/components/StreamingText.tsx';
import ConversationResults from '/src/components/ConversationResults.tsx';
import {CompletedMarkdownCache} from '/src/components/completedMarkdownCache.ts';
window.Cache=CompletedMarkdownCache;
const counters={lex:0,sanitize:0,duration:0};window.counters=counters;
const lexer=marked.lexer;marked.lexer=function(...args){counters.lex++;return lexer.apply(this,args)};
const sanitize=DOMPurify.sanitize;DOMPurify.sanitize=function(...args){counters.sanitize++;return sanitize.apply(this,args)};
const initial=Array.from({length:150},(_,i)=>({id:'reply-'+i,role:'assistant',text:'Reply '+i+'. '+Array.from({length:2},(_,j)=>'Paragraph '+j+'. '+('Completed **text** with [report][ref]. ').repeat(12)).join('\\n\\n')+'\\n\\n[ref]: ./report.md'}));
const other=initial.map(item=>({...item,id:'other-'+item.id,text:'Other chat '+item.text}));
function Harness(){
 const [items,setItems]=useState(initial),[shown,setShown]=useState(true),[agent,setAgent]=useState('first'),[streaming,setStreaming]=useState(false),[results,setResults]=useState(false);
 window.results=()=>flushSync(()=>setResults(true));
 window.chat=otherChat=>flushSync(()=>setItems(otherChat?other:initial));
 window.hide=()=>flushSync(()=>setShown(false));
 window.show=()=>flushSync(()=>setShown(true));
 window.agent=id=>flushSync(()=>setAgent(id));
 window.text=(text,stream=false)=>flushSync(()=>{setItems([{id:'only',role:'assistant',text}]);setStreaming(stream)});
 return <MantineProvider><Profiler id='markdown' onRender={(_id,_phase,d)=>counters.duration+=d}>{shown&&items.map(item=><article key={item.id}><StreamingText text={item.text} streaming={streaming} agentId={agent}/></article>)}</Profiler>{results&&<ConversationResults messages={[{id:'patch',role:'output',toolStatus:'completed',text:JSON.stringify({type:'fileChange',status:'completed',changes:[{path:'file.txt',diff:'@@ -1 +1 @@\\n-old\\n+saved'}]})}]} agentId={agent}/>}</MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Harness/>);`;
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
  const page = await browser.newPage();
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
  await page.waitForFunction(
    () => document.querySelectorAll("article").length === 150,
    null,
    { timeout: 60000 },
  );
  const cold = await page.evaluate(() => ({ ...counters }));
  await page.evaluate(() => window.hide());
  await page.evaluate(() => window.chat(true));
  await page.evaluate(() => window.show());
  await page.evaluate(() => window.hide());
  await page.evaluate(() => window.chat(false));
  await page.evaluate(() =>
    Object.keys(counters).forEach((key) => (counters[key] = 0)),
  );
  await page.evaluate(() => window.show());
  const remount = await page.evaluate(() => ({ ...counters }));
  if (baseline) {
    assert.equal(remount.lex, 150);
    assert.equal(remount.sanitize, cold.sanitize);
    console.log(JSON.stringify({ baseline: true, cold, remount }));
  } else {
    assert.equal(
      remount.lex,
      0,
      "Completed exact text survives component remount",
    );
    assert.equal(remount.sanitize, 0);
    await page.evaluate(() =>
      window.text("[Changed report][r].\n\n[r]: ./changed.md"),
    );
    assert.equal(
      await page.evaluate(() => counters.lex),
      1,
      "Changed text cannot reuse old output",
    );
    await page.getByRole("link", { name: "Changed report" }).waitFor();
    await page.evaluate(() => window.hide());
    await page.evaluate(() => window.agent("second"));
    await page.evaluate(() => window.show());
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
    await page.evaluate(() => window.text("Streaming-only **prefix**.", true));
    await page.evaluate(() => window.hide());
    await page.evaluate(() =>
      Object.keys(counters).forEach((key) => (counters[key] = 0)),
    );
    await page.evaluate(() => window.show());
    assert.equal(
      await page.evaluate(() => counters.lex),
      1,
      "Streaming-only text is never shared",
    );
    await page.evaluate(() =>
      window.text(
        'Safe <img src="bad" onerror="window.executed=true"> [unsafe](javascript:alert(1))',
      ),
    );
    assert.equal(
      await page
        .locator('.prose [onerror],.prose a[href^="javascript:"]')
        .count(),
      0,
    );
    const eviction = await page.evaluate(() => {
      const entries = new Cache(10000, 2);
      entries.set("a", [{ html: "A" }]);
      entries.set("b", [{ html: "B" }]);
      entries.get("a");
      entries.set("c", [{ html: "C" }]);
      const bytes = new Cache(400, 10);
      bytes.set("old", [{ html: "x".repeat(30) }]);
      bytes.set("new", [{ kind: "html", source: "y".repeat(30) }]);
      bytes.set("too-big", [{ html: "x".repeat(1000) }]);
      const blocks = new Cache(400, 10);
      blocks.set(
        "many",
        Array.from({ length: 10 }, () => ({ html: "" })),
      );
      return {
        a: entries.get("a")?.[0].html,
        b: entries.get("b"),
        c: entries.get("c")?.[0].html,
        old: bytes.get("old"),
        new: bytes.get("new")?.[0].source,
        oversized: bytes.get("too-big"),
        many: blocks.get("many"),
      };
    });
    assert.deepEqual(eviction, {
      a: "A",
      b: undefined,
      c: "C",
      old: undefined,
      new: "y".repeat(30),
      oversized: undefined,
      many: undefined,
    });
    assert.deepEqual(errors, []);
    assert.ok(requests.every((request) => request.method === "GET"));
    console.log(JSON.stringify({ browser: browserType.name(), cold, remount }));
    console.log(
      "PASS completed Markdown cache, exact text, current workspace, dialog errors, sanitizer, streaming exclusion and bounded eviction",
    );
  }
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
