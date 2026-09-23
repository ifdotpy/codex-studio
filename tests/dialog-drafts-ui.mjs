#!/usr/bin/env node
// Real components, isolated Vite server, mocked requests. No model or user state.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "dialog-drafts-vite-"));
const entry = join(repo, "web/__ux-fixture.jsx");
const source = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import Picker from '/src/components/ProjectDirectoryPicker.tsx';
import UserMessages from '/src/components/UserMessages.tsx';
import MessageDate from '/src/components/MessageDate.tsx';
import Requests from '/src/components/Requests.tsx';
const lead={id:'lead',rootId:'lead',name:'Lead',isLead:true,source:'managed'};
const complaint={id:'complaint',leadId:'lead',author:'lead',authorName:'Lead',recipient:'user',title:'Help needed',status:'open',needsResponse:true,created:1};
const request={id:'question',agent:'lead',method:'item/tool/requestUserInput',params:{questions:[{id:'public',question:'Public answer'},{id:'secret',question:'Private answer',isSecret:true}]}};
const base={stateDir:'workspace-a',token:'fixture',threads:[lead],runtime:{complaints:[complaint]}};
function Fixture(){const [show,setShow]=useState(false),[workspace,setWorkspace]=useState('workspace-a');
return <MantineProvider><button onClick={()=>setShow(!show)}>Toggle messages</button><button onClick={()=>setWorkspace(workspace==='workspace-a'?'workspace-b':'workspace-a')}>Switch workspace</button>
<div data-missing-date><MessageDate at={0}/></div>
<Requests requests={[request]} allRequests={[request]} scope={workspace} agents={[lead]} refresh={async()=>{}} notify={message=>window.notices.push(message)}/>
<Picker initialPath='/project' onSelect={async()=>{}}/>
{show&&<UserMessages data={{...base,stateDir:workspace}} refresh={async()=>{}} notify={()=>{}}/>}
</MantineProvider>}
window.notices=[];
createRoot(document.getElementById('root')).render(<Fixture/>);`;
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: join(repo, "web"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "dialog-fixture",
      resolveId(id) {
        if (id === "/__ux-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(server) {
        server.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__ux-fixture.jsx"></script>',
          );
        });
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  const pageErrors = [];
  page.on("pageerror", (e) => pageErrors.push(e.message));
  let directoryReads = 0,
    detailReads = 0,
    releaseSend;
  const sent = [];
  const waitForSent = async (count) => {
    for (let attempt = 0; attempt < 100 && sent.length < count; attempt++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert.equal(sent.length, count);
  };
  let message = {
    id: "complaint",
    recipient: "user",
    author: "lead",
    leadId: "lead",
    version: 1,
    text: "Please help",
    responses: [],
    status: "open",
  };
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/directories") {
      directoryReads++;
      return route.fulfill({ json: { path: "/project", directories: [] } });
    }
    if (path === "/api/complaint") {
      detailReads++;
      if (detailReads === 1)
        return route.fulfill({
          status: 503,
          json: { error: "Temporary failure" },
        });
      return route.fulfill({ json: message });
    }
    if (path === "/api/complaints") {
      const body = route.request().postDataJSON();
      sent.push(body);
      if (sent.length === 1)
        return route.fulfill({
          status: 503,
          json: { error: "Lost reply response" },
        });
      await new Promise((resolve) => {
        releaseSend = resolve;
      });
      message = {
        ...message,
        version: message.version + 1,
        status: body.status,
        responses: [
          ...message.responses,
          {
            id: body.id,
            author: "user",
            text: body.text,
            status: body.status,
            at: 1,
          },
        ],
      };
      return route.fulfill({ json: message });
    }
    return route.fulfill({ json: {} });
  });
  await page.goto(server.resolvedUrls.local[0]);
  await page
    .locator("[data-missing-date]")
    .getByText("Date unavailable", { exact: true })
    .waitFor();
  assert.equal(
    await page.locator("[data-missing-date] time").getAttribute("datetime"),
    null,
  );
  const useFolder = page.getByRole("button", {
    name: "Use this folder",
    exact: true,
  });
  await useFolder.waitFor();
  await page.waitForFunction(
    () => !document.querySelector(".directory-footer button")?.disabled,
  );
  await page.getByRole("button", { name: "Go", exact: true }).click();
  await page.waitForFunction(
    () => !document.querySelector(".directory-footer button")?.disabled,
  );
  assert.equal(directoryReads, 2);
  await page.getByRole("button", { name: "Answer", exact: true }).click();
  await page
    .getByLabel("Public answer", { exact: true })
    .fill("Keep the public answer");
  await page
    .getByLabel("Private answer", { exact: true })
    .fill("DO-NOT-STORE-SECRET");
  assert.equal(
    await page.evaluate(() =>
      JSON.stringify(localStorage).includes("DO-NOT-STORE-SECRET"),
    ),
    false,
  );
  await page.reload();
  await page.getByRole("button", { name: "Answer", exact: true }).click();
  assert.equal(
    await page.getByLabel("Public answer", { exact: true }).inputValue(),
    "Keep the public answer",
  );
  assert.equal(
    await page.getByLabel("Private answer", { exact: true }).inputValue(),
    "",
  );
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  await page.getByRole("button", { name: "Answer", exact: true }).click();
  assert.equal(
    await page.getByLabel("Public answer", { exact: true }).inputValue(),
    "",
  );
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  await page.getByRole("button", { name: "Answer", exact: true }).click();
  assert.equal(
    await page.getByLabel("Public answer", { exact: true }).inputValue(),
    "Keep the public answer",
  );
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    window.restoreStorage = () => (Storage.prototype.setItem = original);
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("studio-answer-draft:"))
        throw new DOMException("Full", "QuotaExceededError");
      return original.call(this, key, value);
    };
  });
  await page
    .getByLabel("Public answer", { exact: true })
    .fill("Unsaved answer remains visible");
  assert.equal(
    await page.getByLabel("Public answer", { exact: true }).inputValue(),
    "Unsaved answer remains visible",
  );
  assert.match(
    await page.evaluate(() => window.notices.at(-1)),
    /could not be saved/,
  );
  await page.evaluate(() => window.restoreStorage());
  const toggle = () =>
    page.getByRole("button", { name: "Toggle messages", exact: true }).click();
  await toggle();
  const detail = page.locator('[data-complaint="complaint"]');
  await detail.getByRole("alert").waitFor();
  assert.equal(
    await detail.getByText("Loading message…", { exact: true }).count(),
    0,
  );
  await detail.getByRole("button", { name: "Retry", exact: true }).click();
  await detail.getByText("Please help", { exact: true }).waitFor();
  assert.equal(await detail.getByRole("alert").count(), 0);
  const openReply = () =>
    detail.getByRole("button", { name: "Reply", exact: true }).click();
  const reply = detail.getByLabel("Reply", { exact: true });
  await openReply();
  await reply.fill("Original text");
  await detail
    .getByRole("button", { name: "Close reply", exact: true })
    .click();
  await openReply();
  assert.equal(await reply.inputValue(), "Original text");
  await toggle();
  await toggle();
  await openReply();
  assert.equal(
    await reply.inputValue(),
    "Original text",
    "Reply survives remount",
  );
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  await openReply();
  assert.equal(await reply.inputValue(), "");
  await reply.fill("Separate reply");
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  await openReply();
  assert.equal(
    await reply.inputValue(),
    "Original text",
    "Reply drafts belong to a workspace",
  );
  await page.reload();
  await toggle();
  await openReply();
  assert.equal(
    await reply.inputValue(),
    "Original text",
    "Reply survives reload",
  );
  await detail.getByRole("button", { name: "Send reply", exact: true }).click();
  await detail
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  assert.equal(sent.length, 1);
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  await openReply();
  assert.equal(
    await reply.inputValue(),
    "Separate reply",
    "Another workspace cannot inherit the pending payload",
  );
  assert.equal(await reply.isDisabled(), false);
  assert.equal(
    await detail
      .getByRole("button", { name: "Retry response", exact: true })
      .count(),
    0,
  );
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  await detail
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  assert.equal(await reply.inputValue(), "Original text");
  await page.reload();
  await toggle();
  await detail
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  assert.equal(await reply.inputValue(), "Original text");
  assert.equal(await reply.isDisabled(), true);
  await detail
    .getByRole("button", { name: "Retry response", exact: true })
    .click();
  await waitForSent(2);
  assert.deepEqual(
    sent[1],
    sent[0],
    "Retry after reload retains the exact message identity and payload",
  );
  releaseSend();
  await page.waitForFunction(
    () => !document.querySelector(".complaint-reply textarea")?.disabled,
  );
  assert.equal(await reply.inputValue(), "");
  await detail
    .locator(".complaint-response")
    .getByText("Original text")
    .waitFor();
  await reply.fill("New reply");
  await detail.getByRole("button", { name: "Send reply", exact: true }).click();
  await waitForSent(3);
  assert.equal(await reply.isDisabled(), true);
  releaseSend();
  await detail.locator(".complaint-response").getByText("New reply").waitFor();
  assert.equal(sent[2].text, "New reply");
  assert.equal(sent[0].action, "respond");
  assert.equal(sent[0].complaint_id, "complaint");
  assert.equal(sent[0].version, 1);
  assert.equal(sent[2].version, 2);
  assert.notEqual(sent[0].id, sent[2].id);
  assert.deepEqual(pageErrors, []);
  console.log(
    JSON.stringify({
      ok: true,
      checks: [
        "same-folder retry",
        "message reply collapse, remount and workspace isolation",
        "message detail error and retry",
        "reply draft retained after reopen and reload",
        "non-secret answers survive reload, secrets do not persist",
        "answer workspace isolation and storage failure notice",
        "message reply retry retains its exact identity after reload",
        "reply fixed during send, subsequent response uses new identity and version",
      ],
    }),
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
