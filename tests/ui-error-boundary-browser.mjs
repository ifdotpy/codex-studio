import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "studio-display-boundary-"));
const entry = join(repo, "web/__display-boundary.jsx");
const source = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import Boundary from '/src/components/UIErrorBoundary.tsx';
function Broken({bad}) { if(bad) throw new Error('Fixture display failure'); return <><p>Feature ready</p><input aria-label='Feature input' defaultValue='Feature value'/></>; }
function App() {
  const [bad,setBad]=useState(false),[scope,setScope]=useState('one'),[fatal,setFatal]=useState(false);
  window.fixture={setBad,setScope,setFatal};
  if(fatal) throw new Error('Fixture root failure');
  return <><nav><button onClick={()=>{setBad(false);setScope('two')}}>Other chat</button></nav>
    <input aria-label='Draft' defaultValue='Saved draft'/>
    <Boundary label='this conversation' resetKey={scope}><Broken bad={bad}/></Boundary></>;
}
createRoot(document.getElementById('root')).render(<Boundary label='Studio' fullPage><App/></Boundary>);`;
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: join(repo, "web"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "display-boundary-fixture",
      resolveId(id) {
        if (id === "/__display-boundary.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__display-boundary.jsx"></script>',
          );
        });
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  const useWebkit = process.env.BROWSER === "webkit";
  browser = await (useWebkit ? webkit : chromium).launch({
    headless: true,
    ...(useWebkit
      ? {}
      : {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }),
  });
  const page = await browser.newPage();
  const writes = [];
  page.on("request", (request) => {
    if (request.method() !== "GET") writes.push(request.url());
  });
  await page.goto(server.resolvedUrls.local[0]);
  await page.getByText("Feature ready").waitFor();
  await page
    .getByRole("textbox", { name: "Draft" })
    .fill("Unsent input remains");
  await page.evaluate(() => fixture.setBad(true));
  await page
    .getByRole("alert", { name: "this conversation display error" })
    .waitFor();
  assert.equal(
    await page.getByRole("textbox", { name: "Draft" }).inputValue(),
    "Unsent input remains",
  );
  await page.getByRole("button", { name: "Try again" }).click();
  await page.getByRole("alert").waitFor();
  await page.getByRole("button", { name: "Other chat" }).click();
  await page.getByText("Feature ready").waitFor();
  assert.equal(await page.getByRole("alert").count(), 0);
  assert.equal(
    await page.getByRole("textbox", { name: "Draft" }).inputValue(),
    "Unsent input remains",
  );
  await page.evaluate(() => fixture.setBad(true));
  await page.getByRole("alert").waitFor();
  await page.evaluate(() => fixture.setBad(false));
  await page.getByRole("button", { name: "Try again" }).click();
  await page.getByText("Feature ready").waitFor();
  await page.evaluate(() => {
    localStorage.setItem("retained-draft", "durable input");
    fixture.setFatal(true);
  });
  await page.getByRole("alert", { name: "Studio display error" }).waitFor();
  await page.getByRole("button", { name: "Reload Studio" }).click();
  await page.getByText("Feature ready").waitFor();
  assert.equal(
    await page.evaluate(() => localStorage.getItem("retained-draft")),
    "durable input",
  );
  assert.deepEqual(
    writes,
    [],
    "Recovery never submits messages or changes server state",
  );
  console.log(
    `PASS (${useWebkit ? "WebKit" : "Chromium"}): scoped failure preserves navigation and input, chat switch/retry recover, root fallback reload preserves storage, no writes`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
