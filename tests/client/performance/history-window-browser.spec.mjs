import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.mjs";

test("long history mounts a bounded viewport and preserves navigation and anchors", async ({
  browser,
}) => {
  test.setTimeout(90000);
  const root = fileURLToPath(new URL("../../../web/", import.meta.url));
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-history-window-vite-"));
  const entry = join(root, "history-window-audit.tsx");
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
        name: "history-window-audit",
        configureServer(s) {
          s.middlewares.use("/audit", (_q, r) => {
            r.setHeader("Content-Type", "text/html");
            r.end(
              '<div id="root"></div><script type="module" src="/history-window-audit.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/history-window-audit.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React, { useState, useMemo } from "react";
import { createRoot } from "react-dom/client";
import { flushSync, createPortal } from "react-dom";
import { MantineProvider } from "@mantine/core";
import TurnHistory from "/src/components/conversation/transcript/TurnHistory.tsx";
import StreamingText from "/src/components/conversation/transcript/StreamingText.tsx";
import PromptComposer from "/src/components/prompt-composer/PromptComposer.tsx";
import { useConversationScroll } from "/src/components/useConversationScroll.ts";
import { revealHistoryMessage } from "/src/components/conversation/transcript/historyWindowModel.ts";
import "/src/components/conversation/transcript/turn-history.css";
import "/src/visual-activity.css";
const make = (start, count) => Array.from({length: count}, (_, n) => { const i = start + n; return [
{id: "u"+i, role: "user", text: "Question " + i, turnId: "t"+i, turnStatus: "completed"},
...[0,1,2].map(j => ({id: "tool"+i+"-"+j, sourceId: (i === 500 || i === 0) && j === 1 ? "original-tool"+i+"-1" : undefined, role: "tool", text: JSON.stringify({type: j === 1 ? "commandExecution" : "mcpToolCall", command: j === 1 ? "echo saved" : undefined, status: "completed", tool: "read", arguments: {path:"file-"+i}, result: "Saved output " + i}), turnId: "t"+i, turnStatus: "completed"})),
{id: "a"+i, role: "assistant", text: "Answer " + i + "\\n\\n" + "A paragraph with **formatted text** for stable viewport anchors. ".repeat(10), turnId: "t"+i, turnStatus: "completed", phase: "final_answer"}]; }).flat();
const listeners = new Set(); let draft = "";
const subscribe = (_id, listener) => { listeners.add(listener); return () => listeners.delete(listener); };
const getDraft = () => draft;
const setDraft = value => { draft = value; listeners.forEach(fn => fn()); };
window.renderedMessages = 0;
function PreviewMessage({m}) {const [open,setOpen]=useState(false);return <article data-message={m.id} className="message"><StreamingText text={m.text}/><img alt="Saved attachment" width="60" height="30" src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='60' height='30'%3E%3Crect width='60' height='30' fill='red'/%3E%3C/svg%3E"/><button onClick={()=>setOpen(true)}>Preview attachment</button>{open && createPortal(<div role="dialog"><p>Saved attachment preview</p><button autoFocus onClick={()=>setOpen(false)}>Close preview</button></div>,document.body)}</article>;}
function Harness() {
 const [items, setItems] = useState(() => JSON.parse(localStorage.getItem("fixture-large-history") || "null") || make(0, 1000));
 const [sent, setSent] = useState("");
 const [enabled, setEnabled] = useState(true);
 const sc = useConversationScroll("window-browser-fixture", true);
 window.prepend = () => flushSync(() => setItems(old => [...make(-50,50), ...old]));
 window.latest = () => sc.setFollow(true);
 window.optimistic = many => flushSync(() => {setEnabled(true);setItems([...make(0,many?1000:0), {id:"optimistic:user", clientMessageId:"request", role:"user", text:"Optimistic preview"}]);});
 window.legacy = many => flushSync(() => {setEnabled(false);setItems(make(0,many?1000:1));});
 window.ack = () => flushSync(() => setItems(old => old.map(item => item.clientMessageId === "request" ? {...item,id:"durable-user"}:item)));
 window.prependSameTurn = () => flushSync(() => setItems(old => {const next=[...Array.from({length:50}, (_,i) => ({id:"prefix-"+i,role:"tool",text:JSON.stringify({type:"mcpToolCall",status:"completed",tool:"read",result:"Older output"}),turnId:"large",turnStatus:"completed"})), ...old];localStorage.setItem("fixture-large-history",JSON.stringify(next));return next;}));
 window.small = () => flushSync(() => setItems(make(0,1)));
 window.largeTurn = () => flushSync(() => setItems(Array.from({length:3000}, (_,i) => ({id:"large-"+i, role:"tool", text:JSON.stringify({type:"mcpToolCall",status:"completed",tool:"read",result:"Saved output"}),turnId:"large",turnStatus:"completed"}))));
 window.appendStream = () => flushSync(() => setItems(old => [...old, {id:"stream", role:"assistant", text:"Newest streamed reply", turnId:"live", streaming:true}]));
 window.jump = async (id) => {
   sc.setFollow(false);
   for (let i=0;i<30;i++) {
     revealHistoryMessage(sc.scroll.current, id);
     const target = Array.from(sc.scroll.current.querySelectorAll("[data-message]")).find(node => node.dataset.message === id || node.dataset.sourceMessage === id);
     if (target && !target.hasAttribute("data-lazy-message")) {
       for (let parent=target.parentElement; parent && parent !== sc.scroll.current; parent=parent.parentElement) if (parent.tagName === "DETAILS") parent.open=true;
       sc.scroll.current.scrollTop += target.getBoundingClientRect().top - sc.scroll.current.getBoundingClientRect().top - 16;
       sc.remember(); return;
     }
     if (target) for (let parent=target.parentElement; parent && parent !== sc.scroll.current; parent=parent.parentElement) if (parent.tagName === "DETAILS") parent.open=true;
     await new Promise(resolve => requestAnimationFrame(resolve));
   }
   throw Error("Jump did not find " + id);
 };
 const transcript = useMemo(() => <TurnHistory items={items} enabled={enabled} storageKey="history-window-browser" scrollContainer={sc.scroll} rememberScroll={sc.remember} currentTurn="live" onJump={window.jump} renderMessage={m => {window.renderedMessages++; if(m.id === "a500" || m.clientMessageId === "request") return <PreviewMessage m={m}/>; return <article data-message={m.id} className="message"><StreamingText text={m.text} streaming={m.streaming}/>{m.id === "a500" && <img alt="Saved attachment" width="60" height="30" src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='60' height='30'%3E%3Crect width='60' height='30' fill='red'/%3E%3C/svg%3E"/>}</article>; }}/>, [items]);
 return <MantineProvider><style>{"#messages {height:520px; width:720px; overflow:auto; overflow-anchor:none;} .message {padding:16px; margin:0 0 16px;} p {line-height:24px;} .message-content {min-width:0;}"}</style><div id="messages" ref={sc.scroll} onScroll={sc.onScroll}><div ref={sc.content} className="message-content">{transcript}</div></div><PromptComposer session="fixture" getDraft={getDraft} subscribeDraft={subscribe} onSubmit={e => {e.preventDefault(); setSent(draft);setDraft("");}}>{text => <><textarea id="message" value={text} onChange={e => setDraft(e.target.value)}/><button>Send</button></>}</PromptComposer><output>{sent}</output></MantineProvider>;
}
createRoot(document.getElementById("root")).render(<Harness/>);`;
        },
      },
    ],
  });
  const context = await browser.newContext({
    viewport: { width: 1100, height: 900 },
  });
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/**", (route) => route.fulfill({ json: {} }));
  const mounted = () => page.locator("[data-history-row]").count();
  const offset = (id) =>
    page
      .locator(`[data-message="${id}"]`)
      .evaluate(
        (node) =>
          node.getBoundingClientRect().top -
          document.querySelector("#messages").getBoundingClientRect().top,
      );
  try {
    await server.listen();
    const url = `http://127.0.0.1:${server.httpServer.address().port}/audit`;
    await page.goto(url);
    await expect(page.locator('[data-message="a999"]')).toBeVisible();
    expect(await page.locator("[data-message]").count()).toBeLessThan(150);
    await expect(page.locator("[data-history-window]")).toBeVisible();
    expect(await mounted()).toBeLessThanOrEqual(24);
    expect(await page.evaluate(() => window.renderedMessages)).toBeLessThan(
      150,
    );
    await page.locator("#message").fill("Preserve this draft");
    await page.evaluate(() => window.jump("a500"));
    await expect(page.locator('[data-message="a500"]')).toBeVisible();
    await expect(page.getByAltText("Saved attachment")).toBeVisible();
    await expect
      .poll(async () => Math.abs((await offset("a500")) - 16))
      .toBeLessThan(2);
    const original = await offset("a500");
    await page.evaluate(() => window.prepend());
    await expect
      .poll(async () => Math.abs((await offset("a500")) - original))
      .toBeLessThan(2);
    await page
      .locator("#messages")
      .evaluate((node) => (node.style.width = "510px"));
    await expect
      .poll(async () => Math.abs((await offset("a500")) - original))
      .toBeLessThan(2);
    await page
      .locator("#messages")
      .evaluate((node) => (node.style.width = "720px"));
    await page.evaluate(() => window.jump("original-tool500-1"));
    const tool = page.locator(
      '[data-message="tool500-1"]:not([data-lazy-message])',
    );
    await expect(tool).toBeVisible();
    await tool.locator("summary").first().click();
    await expect(tool).toHaveAttribute("open", "");
    await page.evaluate(() => window.jump("a900"));
    await expect
      .poll(() => page.locator('[data-message="tool500-1"]').count())
      .toBe(0);
    await page.evaluate(() => window.jump("tool500-1"));
    await expect(
      page.locator('[data-message="tool500-1"]:not([data-lazy-message])'),
    ).toHaveAttribute("open", "");
    await page
      .getByRole("button", { name: "Preview attachment", exact: true })
      .click();
    await expect(page.getByRole("dialog")).toBeVisible();
    await page.evaluate(() => window.jump("a900"));
    await expect(page.getByRole("dialog")).toBeVisible();
    await expect(page.locator('[data-message="a500"]')).toBeAttached();
    await page
      .getByRole("button", { name: "Close preview", exact: true })
      .click();
    await page.evaluate(() => window.jump("a500"));
    // A native selection survives eviction of its old viewport.
    await page
      .locator('[data-message="a500"] p')
      .first()
      .evaluate((node) => {
        const range = document.createRange();
        range.selectNodeContents(node);
        const selection = document.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      });
    const selection = await page.evaluate(() =>
      document.getSelection().toString(),
    );
    await page.evaluate(() => window.jump("a900"));
    await expect
      .poll(() => page.evaluate(() => document.getSelection().toString()))
      .toBe(selection);
    expect(await mounted()).toBeLessThanOrEqual(50);
    await page.evaluate(() => document.getSelection().removeAllRanges());
    await page.evaluate(() => window.jump("a500"));
    await expect(page.locator("#message")).toHaveValue("Preserve this draft");
    const savedOffset = await offset("a500");
    await page.waitForTimeout(150);
    await page.reload();
    await expect(page.locator('[data-message="a500"]')).toBeVisible();
    await expect
      .poll(async () => Math.abs((await offset("a500")) - savedOffset))
      .toBeLessThan(2);
    await page.locator("#message").fill("Send after history navigation");
    await page.getByRole("button", { name: "Send", exact: true }).click();
    await expect(page.locator("output")).toHaveText(
      "Send after history navigation",
    );
    await page.evaluate(() => {
      window.latest();
      window.appendStream();
    });
    await expect(page.locator('[data-message="stream"]')).toBeVisible();
    expect(await mounted()).toBeLessThanOrEqual(24);
    const latestRows = await mounted();
    const latestNodes = await page.locator("[data-message]").count();
    await page.evaluate(() => window.largeTurn());
    await expect(page.locator('[data-message="large-2999"]')).toBeAttached();
    expect(await page.locator("[data-message]").count()).toBeLessThanOrEqual(
      512,
    );
    await page.evaluate(() => window.jump("large-20"));
    await expect(
      page.locator('[data-message="large-20"]:not([data-lazy-message])'),
    ).toBeVisible();
    expect(await page.locator(".tool-card").count()).toBeLessThanOrEqual(512);
    const sameTurnTarget = page.locator(
      '[data-message="large-20"]:not([data-lazy-message])',
    );
    await sameTurnTarget.locator("summary").first().click();
    await sameTurnTarget.locator("summary").first().focus();
    await sameTurnTarget
      .locator("summary")
      .first()
      .evaluate((node) => {
        const range = document.createRange();
        range.selectNodeContents(node);
        document.getSelection().removeAllRanges();
        document.getSelection().addRange(range);
        window.sameTurnNode = node;
      });
    const sameTurnSelection = await page.evaluate(() =>
      document.getSelection().toString(),
    );
    const sameTurnOffset = await sameTurnTarget.evaluate(
      (node) =>
        node.getBoundingClientRect().top -
        document.querySelector("#messages").getBoundingClientRect().top,
    );
    await page.evaluate(() => window.prependSameTurn());
    await expect
      .poll(() =>
        page.evaluate(
          () =>
            window.sameTurnNode.isConnected &&
            document.activeElement === window.sameTurnNode,
        ),
      )
      .toBe(true);
    await expect
      .poll(() => page.evaluate(() => document.getSelection().toString()))
      .toBe(sameTurnSelection);
    await expect
      .poll(async () =>
        Math.abs(
          (await sameTurnTarget.evaluate(
            (node) =>
              node.getBoundingClientRect().top -
              document.querySelector("#messages").getBoundingClientRect().top,
          )) - sameTurnOffset,
        ),
      )
      .toBeLessThan(2);
    await page.evaluate(() => document.getSelection().removeAllRanges());
    await page.evaluate(() => window.jump("large-20"));
    const sameTurnReloadOffset = await sameTurnTarget.evaluate(
      (node) =>
        node.getBoundingClientRect().top -
        document.querySelector("#messages").getBoundingClientRect().top,
    );
    await expect
      .poll(() =>
        page.evaluate(() => {
          const anchor = JSON.parse(
            localStorage.getItem("history-window-browser:window-anchor") ||
              "null",
          );
          return anchor?.kind === "message" && anchor.id.startsWith("large-");
        }),
      )
      .toBe(true);
    await page.reload();
    await expect(sameTurnTarget).toBeVisible();
    await expect
      .poll(async () =>
        Math.abs(
          (await sameTurnTarget.evaluate(
            (node) =>
              node.getBoundingClientRect().top -
              document.querySelector("#messages").getBoundingClientRect().top,
          )) - sameTurnReloadOffset,
        ),
      )
      .toBeLessThan(2);
    await page.evaluate(() => window.small());
    await expect(page.locator('[data-message="a0"]')).toBeAttached();
    await expect(page.locator("[data-history-window]")).toHaveCount(0);
    const smallTool = page.locator(
      '[data-message="tool0-0"]:not([data-lazy-message])',
    );
    await smallTool.locator("summary").first().click();
    await expect(smallTool).toHaveAttribute("open", "");
    await page.locator(".turn-work > summary").click();
    await expect(page.locator(".tool-card")).toHaveCount(0);
    await page.locator(".turn-work > summary").click();
    await expect(smallTool).toHaveAttribute("open", "");
    await page.evaluate(() => window.jump("original-tool0-1"));
    await expect(
      page.locator('[data-message="tool0-1"]:not([data-lazy-message])'),
    ).toBeVisible();
    for (const many of [false, true]) {
      await page.evaluate((many) => {
        window.legacy(many);
        window.latest();
      }, many);
      await expect(
        page.locator(`[data-message="a${many ? 999 : 0}"]`),
      ).toBeVisible();
      await page.evaluate(() => window.jump("original-tool0-1"));
      await expect(
        page.locator('[data-message="tool0-1"]:not([data-lazy-message])'),
      ).toBeVisible();
    }
    for (const many of [false, true]) {
      await page.evaluate((many) => {
        window.optimistic(many);
        window.latest();
      }, many);
      const optimistic = page.locator('[data-message="optimistic:user"]');
      await expect(optimistic).toBeVisible();
      await optimistic.evaluate((node) => (window.optimisticNode = node));
      await optimistic
        .getByRole("button", { name: "Preview attachment", exact: true })
        .click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await page.evaluate(() => window.ack());
      await expect(page.getByRole("dialog")).toBeVisible();
      expect(
        await page.evaluate(
          () =>
            window.optimisticNode ===
            document.querySelector('[data-message="durable-user"]'),
        ),
      ).toBe(true);
      await page
        .getByRole("button", { name: "Close preview", exact: true })
        .click();
    }
    expect(errors).toEqual([]);
    console.log(
      JSON.stringify({
        loadedMessages: 5000,
        mountedRows: latestRows,
        mountedMessageNodes: latestNodes,
        renderedMessages: await page.evaluate(() => window.renderedMessages),
      }),
    );
  } finally {
    await context.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
