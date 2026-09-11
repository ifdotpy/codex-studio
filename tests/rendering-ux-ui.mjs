#!/usr/bin/env node
// Isolated component browser checks. No server state or live model requests.
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { writeFile, rm, mkdtemp } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const name = `.rendering-check-${process.pid}`;
const entry = join(root, "web", `${name}.tsx`);
const html = join(root, "web", `${name}.html`);
const artifacts = await mkdtemp(join(tmpdir(), "studio-rendering-theme-"));
let server, browser;
try {
  await writeFile(
    entry,
    `
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import StreamingText from "./src/components/StreamingText";
import Activity from "./src/components/Activity";
import TurnHistory from "./src/components/TurnHistory";
import ComposerAttachments, { MessageAttachments } from "./src/components/ComposerAttachments";
const asset = { id:"fixture-image",name:"pixel.png",mime:"image/png",image:true,size:68,preview:"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=" };
function Harness() {
 const [text, setText] = useState("First sentence. Partial");
 const [count, setCount] = useState(2);
 const [streaming, setStreaming] = useState(true);
 const [theme, setTheme] = useState<"light" | "dark">("light");
 Object.assign(window, { setText, setCount, setStreaming, setTheme });
 const items = Array.from({length:count}, (_,i) => ({id:"tool"+i,turnId:"turn",role:"tool",text:JSON.stringify({type:"commandExecution",command:"pwd",aggregatedOutput:"result"}),createdAt:"2026-09-10"}));
 return <MantineProvider forceColorScheme={theme}><main><div id="text"><StreamingText text={text} streaming={streaming} agentId="fixture" /></div><div id="activity"><Activity items={items}/></div><div id="turn"><TurnHistory items={items} currentTurn="turn" enabled storageKey="test-rendering" renderMessage={()=>null} onJump={()=>{}} /></div><ComposerAttachments notify={()=>{}} assets={[asset]} uploading={false} disabled={false} add={()=>{}} remove={()=>{}}/><MessageAttachments assets={[asset]} notify={()=>{}}/></main></MantineProvider>;
}
createRoot(document.getElementById("root")!).render(<Harness/>);
`,
  );
  await writeFile(
    html,
    `<html><body><div id="root"></div><script type="module" src="/${name}.tsx"></script></body></html>`,
  );
  server = await createServer({
    root: join(root, "web"),
    configFile: false,
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage();
  const errors = [],
    external = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (r.url().includes("external.invalid")) external.push(r.url());
  });
  const pixel =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=";
  await page.route("**/api/file?**", (r) =>
    r.fulfill({
      json: { name: "pixel.png", mime: "image/png", base64: pixel },
    }),
  );
  await page.route("https://external.invalid/**", (r) =>
    r.fulfill({ status: 404, body: "missing" }),
  );
  await page.goto(`${server.resolvedUrls.local[0]}${name}.html`);
  await page.getByText("Partial", { exact: true }).waitFor();
  await page.locator("#text .sentence-enter").count();
  await page.evaluate(() => {
    const span = [...document.querySelectorAll("#text span")].find(
      (n) => n.textContent === "First sentence. ",
    );
    window.retained = span;
    const range = document.createRange();
    range.selectNodeContents(span);
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    window.setText("First sentence. Partial response continues");
  });
  await page.getByText("Partial response continues", { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.retained.isConnected), true);
  assert.equal(
    await page.evaluate(() => window.getSelection().toString()),
    "First sentence. ",
  );
  for (const id of ["activity", "turn"]) {
    await page
      .locator(`#${id} .tool-card`)
      .first()
      .locator("summary")
      .first()
      .click();
    await page.locator(`#${id} .tool-body`).first().waitFor();
    await page
      .locator(`#${id} .tool-card`)
      .first()
      .evaluate((n) => (n.dataset.retained = "yes"));
  }
  await page.evaluate(() => window.setCount(3));
  for (const id of ["activity", "turn"]) {
    await page.locator(`#${id} .tool-card`).nth(2).waitFor();
    assert.equal(
      await page
        .locator(`#${id} .tool-card`)
        .first()
        .getAttribute("data-retained"),
      "yes",
    );
    assert.notEqual(
      await page.locator(`#${id} .tool-card`).first().getAttribute("open"),
      null,
    );
    const group = page.locator(`#${id} > details, #${id} .turn-work`).first();
    await group.locator(":scope > summary").click();
    await group.locator(":scope > summary").click();
    assert.notEqual(
      await page.locator(`#${id} .tool-card`).first().getAttribute("open"),
      null,
    );
  }
  await page.evaluate(() => window.setText("```python\nprint('partial"));
  await page
    .locator(".code-block code")
    .getByText("print('partial", { exact: false })
    .waitFor();
  await page.getByRole("button", { name: "Expand code" }).click();
  assert.equal(
    await page.locator(".code-block-content").getAttribute("data-expanded"),
    "true",
  );
  await page.evaluate(() =>
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: async () => {
          throw Error("denied");
        },
      },
    }),
  );
  await page.getByRole("button", { name: "Copy code", exact: true }).click();
  await page.getByText("Cannot copy. Select the code to copy it.").waitFor();
  await page.evaluate(() => window.setText("```mermaid\nflowchart LR\nA-->B"));
  await page
    .locator(".code-block code")
    .getByText("A-->B", { exact: false })
    .waitFor();
  assert.equal(await page.locator(".rich-preview").count(), 0);
  await page.evaluate(() =>
    window.setText("```mermaid\nflowchart LR\nA-->B\n```"),
  );
  await page.frameLocator(".rich-preview iframe").locator("svg").waitFor();
  await page.getByRole("button", { name: "Copy source", exact: true }).click();
  await page
    .getByText("Cannot copy. Open Source and select the text to copy it.")
    .waitFor();
  assert.equal(await page.locator(".rich-preview iframe").count(), 1);
  await page.evaluate(() =>
    window.setText(
      "![Remote](https://external.invalid/pixel.png)\n\n![Local](./pixel.png)",
    ),
  );
  await page.getByRole("button", { name: "Load image", exact: true }).waitFor();
  assert.deepEqual(external, [], "remote images require explicit load");
  await page
    .locator("#text")
    .getByRole("button", { name: "Preview Local" })
    .waitFor();
  await page.getByRole("button", { name: "Load image", exact: true }).click();
  await page.getByText("Cannot load this image.").waitFor();
  assert.equal(external.length, 1);
  for (const selector of [
    ".attachment-preview-button",
    ".message-attachments .image-preview-button",
    "#text .image-preview-button",
  ]) {
    await page.locator(selector).click();
    await page.getByRole("dialog").waitFor();
    await page.locator(".workspace-image").waitFor();
    await page.keyboard.press("Escape");
    await page.getByRole("dialog").waitFor({ state: "hidden" });
  }
  await page.evaluate(() =>
    window.setText(
      "```python\nprint('theme')\n```\n\n```html\n<p>Preview artifact</p>\n```",
    ),
  );
  await page.locator(".rich-preview iframe").waitFor();
  for (const theme of ["light", "dark"]) {
    await page.evaluate((value) => window.setTheme(value), theme);
    await page.waitForFunction(
      (value) => document.documentElement.dataset.mantineColorScheme === value,
      theme,
    );
    const colors = await page.evaluate(() => {
      const read = (selector) =>
        getComputedStyle(document.querySelector(selector));
      return {
        background: read(".rich-preview").backgroundColor,
        label: read(".rich-preview-label").color,
        codeBackground: read(".code-block").backgroundColor,
        codeText: read(".code-block").color,
      };
    });
    const luminance = (color) => {
      const rgb = color
        .match(/[\d.]+/g)
        .slice(0, 3)
        .map(Number)
        .map((n) => n / 255)
        .map((n) => (n <= 0.04045 ? n / 12.92 : ((n + 0.055) / 1.055) ** 2.4));
      return rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
    };
    const contrast = (a, b) =>
      (Math.max(luminance(a), luminance(b)) + 0.05) /
      (Math.min(luminance(a), luminance(b)) + 0.05);
    assert.ok(
      contrast(colors.background, colors.label) >= 4.5,
      `${theme} preview label contrast`,
    );
    assert.ok(
      contrast(colors.codeBackground, colors.codeText) >= 4.5,
      `${theme} code contrast`,
    );
    assert.equal(luminance(colors.background) > 0.5, theme === "light");
    await page.screenshot({
      path: join(artifacts, `${theme}.png`),
      fullPage: true,
    });
  }
  console.log(`Theme screenshots: ${artifacts}`);
  assert.deepEqual(errors, []);
  console.log(
    "PASS: partial text/code; selection; tool expansion; code controls; copy failure; image privacy, failure, and previews",
  );
} finally {
  await browser?.close();
  await server?.close();
  await rm(entry, { force: true });
  await rm(html, { force: true });
}
