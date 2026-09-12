import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web"),
  require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-file-viewers-"));
const entry = join(root, "file-viewers-fixture.tsx");
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
      "react-dom/client",
      "@mantine/core",
      "dompurify",
      "marked",
    ],
  },
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/file-viewers-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/file-viewers-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import FilePreview from '/src/components/FilePreview.tsx';import StreamingText from '/src/components/StreamingText.tsx';import {parseDelimited,safeSvg} from '/src/components/filePreviewFormats.ts';window.parseDelimited=parseDelimited;window.safeSvg=safeSvg;window.actions=[];window.codexDesktop={fileAction:async value=>{window.actions.push(value);return true},saveFile:async value=>{window.saved={name:value.name,bytes:Array.from(new Uint8Array(value.data))};return true}};function App(){const [target,setTarget]=useState(null);window.show=(path,line)=>setTarget({agent:'agent-a',path,line});return <MantineProvider><StreamingText agentId="agent-a" text="[Open document](/docs/readme.md)"/><FilePreview target={target} onClose={()=>setTarget(null)}/></MantineProvider>}createRoot(document.getElementById('root')).render(<App/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
  browser = await engine.launch({
    headless: true,
    ...(engine === chromium
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
  page.on("pageerror", (e) => errors.push(e.message));
  const fixtures = {
    "/docs/readme.md": [
      "text/markdown",
      "# Title\n\n**Bold** and [next](chapter.md#L2)\n\n![Diagram](image.svg)\n\n[table](table.csv)\n\n[bad link](./bad%ZZ.md)\n\n![Inline](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=)",
    ],
    "/docs/chapter.md": ["text/markdown", "first\nsecond\nthird"],
    "/docs/image.svg": [
      "image/svg+xml",
      `<svg xmlns="http://www.w3.org/2000/svg" width="960" height="640" onload="window.svgExecuted=true">
<style>@import url('https://blocked.invalid/import.css'); @font-face {font-family:external;src:url('https://blocked.invalid/font.woff2')} .red {fill:rgb(255,0,0)} .gradient {fill:url('#gradient')} .external {fill:url('https://blocked.invalid/external.svg#paint')}</style>
<defs><linearGradient id="gradient"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient></defs>
<rect width="960" height="640" fill="white"/>
<rect class="red" width="100" height="100"/>
<rect x="100" width="100" height="100" style="fill:rgb(0,128,0)"/>
<rect x="200" width="200" height="100" fill="url('#gradient')"/>
<rect x="400" width="200" height="100" class="gradient"/>
<rect x="600" width="200" height="100" style="fill:url(&quot;#gradient&quot;)"/>
<rect class="external" x="800" width="100" height="100"/><text y="200" style="font-family:external">External font must not load</text>
<script>window.svgExecuted=true</script><image href="https://blocked.invalid/x"/><foreignObject><p>Bad</p></foreignObject></svg>`,
    ],
    "/docs/table.csv": ["text/csv", "nested,table\n1,2"],
    "/data.csv": [
      "text/csv",
      'name,note\r\n"a,b","line1\nline2"\r\n"quote""value",=SUM(A1)',
    ],
    "/data.tsv": ["text/tab-separated-values", 'a\tb\n"c\td"\te'],
    "/data.json": [
      "application/json",
      '{"object":{"value":1},"array":[true,null]}',
    ],
    "/broken.json": ["application/json", "{oops"],
    "/sound.wav": ["audio/wav", ""],
    "/movie.mp4": ["video/mp4", ""],
    "/page.html": [
      "text/html",
      "<h1>HTML</h1><script>window.bad=true</script>",
    ],
    "/document.pdf": ["application/pdf", "%PDF-1.4"],
    "/other.docx": [
      "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
      "binary",
    ],
  };
  let delayed;
  await page.route("**/api/file?*", async (route) => {
    const url = new URL(route.request().url()),
      path = url.searchParams.get("path");
    requests.push(path);
    if (path === "/late.md") {
      delayed = route;
      return;
    }
    if (path === "/large.docx") {
      await route.fulfill({
        status: 400,
        json: { error: "File exceeds preview limit" },
      });
      return;
    }
    const value = fixtures[path];
    if (!value) {
      await route.fulfill({ status: 404, json: { error: "Missing " + path } });
      return;
    }
    await route.fulfill({
      json: {
        name: path.split("/").at(-1),
        mime: value[0],
        base64: Buffer.from(value[1]).toString("base64"),
      },
    });
  });
  const externalRequests = [];
  await page.route("https://blocked.invalid/**", async (route) => {
    externalRequests.push(route.request().url());
    await route.abort();
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.waitForFunction(() => window.show);
  const show = async (path) => {
    await page.evaluate((path) => window.show(path), path);
    await page.waitForFunction(
      (name) =>
        document.querySelector(".mantine-Modal-title")?.textContent === name,
      path.split("/").at(-1),
    );
    await page
      .getByRole("button", { name: "Save a copy", exact: true })
      .waitFor();
  };
  // Conditionally mounted previews capture the trigger even on their first open.
  for (let attempt = 0; attempt < 2; attempt++) {
    const trigger = page.getByRole("link", {
      name: "Open document",
      exact: true,
    });
    await trigger.focus();
    await trigger.press("Enter");
    await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
    await page.keyboard.press("Escape");
    await page.waitForFunction(
      () => !document.querySelector('[role="dialog"]'),
    );
    await page.waitForFunction(
      () => document.activeElement?.textContent === "Open document",
      null,
      { timeout: 3000 },
    );
  }
  // Exercise React portal bubbling from a file opened by transcript Markdown.
  await page.getByRole("link", { name: "Open document", exact: true }).click();
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await page.getByRole("link", { name: "table", exact: true }).focus();
  await page.getByRole("link", { name: "table", exact: true }).press("Enter");
  await page.waitForFunction(
    () =>
      document.querySelector('table[aria-label="File table"]') ||
      document.querySelector('[role="alert"]'),
  );
  assert(
    !requests.includes("table.csv"),
    "Outer Markdown must not reinterpret the nested link relative to agent cwd",
  );
  assert.equal(requests.filter((path) => path === "/docs/table.csv").length, 1);
  await page.getByRole("table", { name: "File table" }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await page.waitForFunction(
    () => document.activeElement?.textContent === "table",
    null,
    { timeout: 3000 },
  );
  await page.getByRole("link", { name: "bad link", exact: true }).focus();
  await page
    .getByRole("link", { name: "bad link", exact: true })
    .press("Enter");
  await page
    .getByRole("dialog", { name: "Cannot open file", exact: true })
    .waitFor();
  assert.equal(
    await page.getByRole("alert").count(),
    1,
    "Only the nearest Markdown handles a malformed local link",
  );
  await page.keyboard.press("Escape");
  await page
    .getByRole("dialog", { name: "Cannot open file", exact: true })
    .waitFor({ state: "hidden" });
  await page.waitForFunction(
    () => document.activeElement?.textContent === "bad link",
    null,
    { timeout: 3000 },
  );
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.querySelector('[role="dialog"]'));
  await show("/docs/readme.md");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await page
    .getByRole("button", { name: "Preview Diagram", exact: true })
    .waitFor();
  assert(requests.includes("/docs/image.svg"));
  await page
    .getByRole("button", { name: "Preview Diagram", exact: true })
    .click();
  await page.getByRole("region", { name: "Image viewer" }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await page.getByRole("link", { name: "table", exact: true }).click();
  await page.getByRole("table", { name: "File table" }).waitFor();
  assert.equal(await page.locator("td").first().textContent(), "nested");
  await page.keyboard.press("Escape");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await page
    .getByRole("button", { name: "Preview Inline", exact: true })
    .click();
  await page.getByRole("region", { name: "Image viewer" }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  // Changing the outer target unmounts an open inner dialog. Its stack ID must
  // be retired, or the new outer file cannot receive Escape or focus.
  await page
    .getByRole("button", { name: "Preview Diagram", exact: true })
    .click();
  await page.getByRole("region", { name: "Image viewer" }).waitFor();
  await show("/data.json");
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.querySelector('[role="dialog"]'));
  await show("/docs/readme.md");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();

  await page.getByRole("button", { name: "Source", exact: true }).click();
  assert(
    (await page.locator(".file-preview-source").textContent()).includes(
      "# Title",
    ),
  );
  await page.getByRole("button", { name: "Preview", exact: true }).click();
  await page.getByRole("link", { name: "next", exact: true }).click();
  await page.locator('[data-file-line="2"]').waitFor();
  assert.equal(
    await page.locator('[data-file-line="2"]').textContent(),
    "second",
  );
  assert(requests.includes("/docs/chapter.md"));
  // Close the nested file without closing the original Markdown file.
  await page.keyboard.press("Escape");
  await page.getByRole("heading", { name: "Title", exact: true }).waitFor();
  await show("/data.csv");
  await page.getByRole("table").waitFor();
  assert.equal(await page.locator("td").nth(2).textContent(), "a,b");
  assert.equal(await page.locator("td").nth(3).textContent(), "line1\nline2");
  assert.equal(await page.locator("td").nth(4).textContent(), 'quote"value');
  assert.equal(await page.locator("td").nth(5).textContent(), "=SUM(A1)");
  await page.getByRole("button", { name: "Save a copy", exact: true }).click();
  assert.deepEqual(
    await page.evaluate(() => window.saved.bytes),
    Array.from(Buffer.from(fixtures["/data.csv"][1])),
  );
  await show("/data.tsv");
  assert.equal(await page.locator("td").nth(2).textContent(), "c\td");
  await show("/data.json");
  assert(
    (await page.locator(".file-preview-source").textContent()).includes(
      '\n  "object": {',
    ),
  );
  await page.getByRole("button", { name: "Source", exact: true }).click();
  assert.equal(
    await page.locator(".file-preview-source").textContent(),
    fixtures["/data.json"][1],
  );
  await show("/broken.json");
  await page.getByText("Invalid JSON. The original source is shown.").waitFor();
  await show("/docs/image.svg");
  await page.waitForFunction(
    () => document.querySelector(".image-viewer img")?.naturalWidth === 960,
  );
  const pixels = await page.evaluate(async () => {
    // Use the displayed image bytes at their intrinsic size. WebKit couples SVG
    // rasterization to an img element's fitted CSS dimensions.
    const image = new Image();
    image.src = document.querySelector(".image-viewer img").src;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    const context = canvas.getContext("2d");
    context.drawImage(image, 0, 0, image.naturalWidth, image.naturalHeight);
    return [50, 150, 220, 380, 420, 580, 620, 780].map((x) =>
      Array.from(context.getImageData(x, 50, 1, 1).data),
    );
  });
  assert.deepEqual(pixels[0], [255, 0, 0, 255], "SVG class style survives");
  assert.deepEqual(pixels[1], [0, 128, 0, 255], "SVG inline style survives");
  for (let i = 2; i < pixels.length; i += 2) {
    assert(
      pixels[i][0] > 200 && pixels[i][2] < 55,
      "Quoted local gradient starts red",
    );
    assert(
      pixels[i + 1][2] > 200 && pixels[i + 1][0] < 55,
      "Quoted local gradient ends blue",
    );
  }
  assert.deepEqual(
    externalRequests,
    [],
    "SVG image cannot load external resources",
  );
  await page.getByRole("button", { name: "Actual size", exact: true }).click();
  assert.equal(
    await page.getByLabel("Image zoom", { exact: true }).textContent(),
    "100%",
  );
  await page.getByRole("button", { name: "Zoom in", exact: true }).click();
  assert.equal(
    await page.getByLabel("Image zoom", { exact: true }).textContent(),
    "125%",
  );
  await page.locator(".image-viewer-viewport").focus();
  await page.keyboard.press("1");
  assert.equal(
    await page.getByLabel("Image zoom", { exact: true }).textContent(),
    "100%",
  );
  assert.equal(await page.evaluate(() => window.svgExecuted), undefined);
  await show("/sound.wav");
  assert.equal(await page.locator("audio[controls]").count(), 1);
  await page
    .locator("audio")
    .evaluate((node) => node.dispatchEvent(new Event("error")));
  await page
    .getByText("Cannot play this audio format.", { exact: true })
    .waitFor();
  await show("/movie.mp4");
  assert.equal(await page.locator("video[controls][playsinline]").count(), 1);
  await page
    .locator("video")
    .evaluate((node) => node.dispatchEvent(new Event("error")));
  await page
    .getByText("Cannot play this video format.", { exact: true })
    .waitFor();
  await show("/page.html");
  assert.equal(await page.locator("iframe").getAttribute("sandbox"), "");
  assert(
    !(await page.locator("iframe").getAttribute("srcdoc")).includes("<script>"),
  );
  await show("/document.pdf");
  assert(
    (await page.locator("iframe").getAttribute("src")).startsWith("blob:"),
  );
  await show("/other.docx");
  await page.getByText("Preview is unavailable", { exact: false }).waitFor();
  await page.evaluate(() => window.show("/large.docx"));
  await page.getByText("File exceeds preview limit", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Quick Look", exact: true }).click();
  assert.deepEqual((await page.evaluate(() => window.actions)).at(-1), {
    action: "preview",
    target: { agent: "agent-a", path: "/large.docx" },
  });
  await page.evaluate(() => window.show("/late.md"));
  await page.waitForFunction(() =>
    document.querySelector('[aria-label="Load file"]'),
  );
  await show("/data.json");
  await delayed.fulfill({
    json: {
      name: "late.md",
      mime: "text/markdown",
      base64: Buffer.from("# Stale").toString("base64"),
    },
  });
  await page.waitForTimeout(100);
  assert.equal(
    await page.getByRole("heading", { name: "Stale", exact: true }).count(),
    0,
  );
  const parser = await page.evaluate(() => ({
    limit: window.parseDelimited("a\n".repeat(250), ","),
    broken: window.parseDelimited('"missing', ","),
    columns: window.parseDelimited("a,b,c", ",", 200, 2),
    svg: window.safeSvg(
      '<svg xmlns="http://www.w3.org/2000/svg"><style>*{fill:url(https://bad/x)}</style><rect onclick="evil()" fill="url(https://bad/x)"/></svg>',
    ),
  }));
  assert.equal(parser.limit.rows.length, 200);
  assert(parser.limit.truncated);
  assert(parser.broken.error);
  assert(parser.columns.truncated);
  assert(!/onclick|<script|foreignObject/.test(parser.svg));
  assert(
    parser.svg.includes("<style>"),
    "SVG CSS is preserved for image-only rendering",
  );
  assert.deepEqual(externalRequests, []);
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      browser: engine.name(),
      result: "PASS",
      requests: requests.length,
      checks: [
        "Markdown and relative links/images",
        "exact source and line",
        "CSV/TSV quoted rows and bounds",
        "JSON",
        "safe SVG and zoom keyboard",
        "media/PDF/HTML",
        "native oversized action",
        "stale response",
      ],
    }),
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
