import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-render-audit-"));
const entry = join(root, "transcript-audit.tsx");
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
      name: "audit",
      configureServer(s) {
        s.middlewares.use("/audit", (_q, r) => {
          r.setHeader("Content-Type", "text/html");
          r.end(
            '<div id="root"></div><script type="module" src="/transcript-audit.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/transcript-audit.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `import React, { useState, Profiler } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import { marked } from "marked";
import DOMPurify from "dompurify";
import TurnHistory from "/src/components/TurnHistory.tsx";
import StreamingText from "/src/components/StreamingText.tsx";
import { NativeNotice } from "/src/components/NativeNotice.tsx";
const counters = {
  lex: 0,
  lexChars: 0,
  sanitize: 0,
  sanitizeChars: 0,
  toolParse: 0,
  renders: 0,
  duration: 0,
};
window.counters = counters;
const lex = marked.lexer;
marked.lexer = function (t, ...args) {
  counters.lex++;
  counters.lexChars += t.length;
  return lex.call(this, t, ...args);
};
const sanitize = DOMPurify.sanitize;
DOMPurify.sanitize = function (t, ...args) {
  counters.sanitize++;
  counters.sanitizeChars += t.length;
  return sanitize.call(this, t, ...args);
};
const parse = JSON.parse;
JSON.parse = function (t, ...args) {
  if (typeof t === "string" && t.includes("audit_tool_payload"))
    counters.toolParse++;
  return parse.call(this, t, ...args);
};
const prose = Array.from(
  { length: 10 },
  (_, i) =>
    "Paragraph " +
    i +
    ". " +
    "Text with **strong words** and [source](./src/main.ts). ".repeat(12),
).join("\\n\\n");
const tooltext = JSON.stringify({
  type: "commandExecution",
  status: "completed",
  command: "audit_tool_payload",
  aggregatedOutput: "x".repeat(8192),
});
const initial = Array.from({ length: 30 }, (_, i) => [
  {
    id: "u" + i,
    role: "user",
    text: "Question",
    turnId: "t" + i,
    turnStatus: "completed",
  },
  ...Array.from({ length: 3 }, (_, j) => ({
    id: "tool" + i + "-" + j,
    role: "output",
    text: tooltext,
    turnId: "t" + i,
    turnStatus: "completed",
    toolStatus: "completed",
  })),
  {
    id: "a" + i,
    role: "assistant",
    text: prose,
    turnId: "t" + i,
    turnStatus: "completed",
    phase: "final_answer",
  },
]).flat();
const render = (m) =>
  m.nativeNotice ? (
    <NativeNotice item={m} />
  ) : (
    <StreamingText text={m.text} streaming={m.streaming} agentId="fixture" />
  );
const jump = () => {};
function Harness() {
  const [tick, setTick] = useState(0);
  const [mode, setMode] = useState("history");
  const [suffix, setSuffix] = useState("");
  const [items, setItems] = useState(initial);
  const [stream, setStream] = useState(null);
  window.replaceLast = () =>
    flushSync(() =>
      setItems((old) =>
        old.map((m) =>
          m.id === "a29"
            ? { ...m, text: m.text + "\\n\\n[Updated report](./changed.md)" }
            : m,
        ),
      ),
    );
  window.failTool = () =>
    flushSync(() =>
      setItems((old) =>
        old.map((m) =>
          m.id === "tool29-0"
            ? {
                ...m,
                toolStatus: "failed",
                text: JSON.stringify({
                  type: "commandExecution",
                  status: "failed",
                  error: {
                    message: "Usage limit reached",
                    codexErrorInfo: "usageLimitExceeded",
                  },
                }),
              }
            : m,
        ),
      ),
    );
  window.diagnostic = (error) =>
    flushSync(() =>
      setItems((old) =>
        old.map((m) =>
          m.id === "a29" ? { ...m, turnStatus: "failed", turnError: error } : m,
        ),
      ),
    );
  window.stream = (text) => flushSync(() => setStream(text));
  window.tick = () => flushSync(() => setTick((v) => v + 1));
  window.mode = (v) => flushSync(() => setMode(v));
  window.append = () =>
    flushSync(() => setSuffix((v) => v + " Next sentence."));
  return (
    <MantineProvider>
      <input value={tick} readOnly />
      <Profiler
        id="history"
        onRender={(_id, _phase, d) => {
          counters.renders++;
          counters.duration += d;
        }}
      >
        {mode === "history" ? (
          <TurnHistory
            items={items.slice()}
            enabled
            storageKey="audit-only"
            renderMessage={(m) => render(m)}
            onJump={jump}
          />
        ) : (
          <StreamingText
            text={stream ?? prose + suffix}
            streaming
            agentId="fixture"
          />
        )}
      </Profiler>
    </MantineProvider>
  );
}
createRoot(document.getElementById("root")).render(<Harness />);
`;
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
  const errors = [];
  page.on("pageerror", (e) => {
    errors.push(e.message);
    console.error("Browser error:", e.message);
  });
  await page.route("**/api/**", (r) => r.fulfill({ json: {} }));
  await page.goto(
    "http://127.0.0.1:" + server.httpServer.address().port + "/audit",
  );
  await page.waitForSelector(".turn-history");
  await page.waitForTimeout(250);
  const reset = () =>
    page.evaluate(() =>
      Object.keys(counters).forEach((k) => (counters[k] = 0)),
    );
  await page
    .locator(".turn-history")
    .first()
    .evaluate((el) => (window.originalTurn = el));
  await reset();
  await page.evaluate(() => {
    for (let i = 0; i < 10; i++) window.tick();
  });
  const unchanged = await page.evaluate(() => ({ ...counters }));
  assert.equal(
    unchanged.lex,
    0,
    "Completed replies do not reparse on parent updates",
  );
  assert.equal(
    unchanged.toolParse,
    0,
    "Unchanged tool diagnostics do not reparse",
  );
  assert.equal(unchanged.sanitize, 0);
  await page.evaluate(() => window.replaceLast());
  await page.getByRole("button", { name: "Updated report" }).waitFor();
  const changed = await page.evaluate(() => ({ ...counters }));
  assert.equal(
    changed.lex,
    2,
    "Only the changed reply and its results reparse",
  );
  assert.equal(
    await page
      .locator(".turn-history")
      .first()
      .evaluate((el) => el === window.originalTurn),
    true,
    "Earlier turn retains its DOM node",
  );
  await page.evaluate(() => window.failTool());
  await page.getByText("Account limit reached", { exact: false }).waitFor();
  await page.evaluate(() =>
    window.diagnostic({ message: "First diagnostic", code: "first" }),
  );
  await page.getByText("First diagnostic", { exact: false }).first().waitFor();
  await page.evaluate(() =>
    window.diagnostic({ message: "Updated diagnostic", code: "second" }),
  );
  await page
    .getByText("Updated diagnostic", { exact: false })
    .first()
    .waitFor();
  assert.equal(
    await page.getByText("First diagnostic", { exact: false }).count(),
    0,
  );
  await page.evaluate(() => window.mode("stream"));
  await reset();
  await page.evaluate(() => {
    for (let i = 0; i < 20; i++) window.append();
  });
  const streaming = await page.evaluate(() => ({ ...counters }));
  assert.equal(
    streaming.lex,
    20,
    "Each stream update still parses full Markdown",
  );
  assert.equal(
    streaming.sanitize,
    20,
    "Only changed final HTML requires sanitizing",
  );
  await page.evaluate(() =>
    window.stream("Read [report][ref].\n\nStable paragraph."),
  );
  await page
    .getByText("Stable paragraph.", { exact: true })
    .evaluate((el) => (window.originalSentence = el));
  await page.evaluate(() =>
    window.stream(
      "Read [report][ref].\n\nStable paragraph.\n\n[ref]: ./first.md",
    ),
  );
  assert.equal(
    await page.getByRole("link", { name: "report" }).getAttribute("href"),
    "./first.md",
  );
  assert.equal(
    await page
      .getByText("Stable paragraph.", { exact: true })
      .evaluate((el) => el === window.originalSentence),
    true,
  );
  await page.evaluate(() =>
    window.stream(
      "Read [report][ref].\n\nStable paragraph.\n\n[ref]: ./second.md",
    ),
  );
  assert.equal(
    await page.getByRole("link", { name: "report" }).getAttribute("href"),
    "./second.md",
    "Later reference definition updates earlier block",
  );
  await page.evaluate(() => window.stream("```ts\nconst x = 1;"));
  await page.locator(".prose pre").waitFor();
  await page.evaluate(() =>
    window.stream("```ts\nconst x = 1;\n```\n\nAfter code."),
  );
  await page.getByText("After code.", { exact: true }).waitFor();
  assert.equal(await page.locator(".prose pre").count(), 1);
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      browser: browserType.name(),
      unchanged,
      changed,
      streaming,
    }),
  );
  console.log(
    "PASS transcript parsing cache, changed turns, native diagnostics, reference links, open fences and retained DOM",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
