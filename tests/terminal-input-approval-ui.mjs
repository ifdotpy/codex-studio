// Exercise the Codex 0.159 write_stdin_approval request through the real card and answer API.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const temporary = await mkdtemp(join(tmpdir(), "studio-stdin-approval-"));
const harness = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import Requests from '/src/components/Requests.tsx';
const agent={id:'lead',rootId:'lead',name:'Lead',epoch:0,accountKey:'default',turnId:'turn',inFlight:true};
const request={id:'request-159',agent:'lead',method:'item/commandExecution/requestApproval',params:{kind:'writeStdin',threadId:'thread-159',turnId:'turn-159',itemId:'exec-159',startedAtMs:1750000000000,approvalId:'stdin-approval-159',reason:'Send input to an existing terminal to continue the reviewed command.'}};
createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,null,React.createElement(Requests,{requests:[request],allRequests:[request],scope:'fixture',agents:[agent],refresh:async()=>{},notify:()=>{}})));
`;
const server = await createServer({
  configFile: false,
  root: join(root, "web"),
  cacheDir: join(temporary, "vite"),
  plugins: [{
    name: "stdin-approval-fixture",
    resolveId(id) { return id === "virtual:stdin-approval" ? "\0" + id : undefined; },
    load(id) { return id === "\0virtual:stdin-approval" ? harness : undefined; },
  }],
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
await server.listen();
let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath: process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage();
  page.setDefaultTimeout(5000);
  const answers = [];
  await page.route("**/api/**", async (route) => {
    if (route.request().method() === "POST") {
      answers.push(route.request().postDataJSON());
      return route.fulfill({ json: { status: "answered" } });
    }
    return route.fulfill({ json: {} });
  });
  await page.route("**/check", (route) => route.fulfill({
    contentType: "text/html",
    body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__virtual:stdin-approval";</script>',
  }));
  await page.goto(server.resolvedUrls.local[0] + "check");
  await page.waitForFunction(() => document.querySelector('[data-request="request-159"]'));
  await page.getByText("Terminal input approval required", { exact: true }).waitFor();
  await page.getByText("Send input to an existing terminal to continue the reviewed command.", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Allow input", exact: true }).click();
  const deadline = Date.now() + 5000;
  while (!answers.length && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 10));
  assert.deepEqual(answers, [{ decision: "accept", id: "request-159" }]);
  console.log("PASS: 0.159 writeStdin request renders its reason and submits one approval reply");
} finally {
  await browser?.close();
  await server.close();
  await rm(temporary, { recursive: true, force: true });
}
