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
import UserTasks from '/src/components/UserTasks.tsx';
import UserMessages from '/src/components/UserMessages.tsx';
const lead={id:'lead',rootId:'lead',name:'Lead',isLead:true,source:'managed'};
const task={id:'task',agent:'lead',rootId:'lead',title:'Check release',description:'Check it',criteria:'Confirmed',status:'open',version:1};
const complaint={id:'complaint',leadId:'lead',author:'lead',authorName:'Lead',recipient:'user',title:'Help needed',status:'open',needsResponse:true,created:1};
const base={stateDir:'workspace-a',token:'fixture',threads:[lead],runtime:{userTasks:[task],complaints:[complaint]}};
function Fixture(){const [show,setShow]=useState(true),[compact,setCompact]=useState(true),[workspace,setWorkspace]=useState('workspace-a');
return <MantineProvider><button onClick={()=>setShow(!show)}>Toggle tasks</button><button onClick={()=>setCompact(!compact)}>Toggle compact</button><button onClick={()=>setWorkspace(workspace==='workspace-a'?'workspace-b':'workspace-a')}>Switch workspace</button>
<Picker initialPath='/project' onSelect={async()=>{}}/>
{show&&<UserTasks data={{...base,stateDir:workspace}} agent={lead} compact={compact} refresh={async()=>{}} notify={()=>{}}/>}
<UserMessages data={base} refresh={async()=>{}} notify={()=>{}}/>
</MantineProvider>}
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
  const taskSection = page.locator(".user-tasks");
  const expand = () =>
    taskSection.getByRole("button", { name: /Your tasks/ }).click();
  const openTask = () =>
    taskSection
      .getByRole("button", { name: "Check release", exact: true })
      .click();
  await expand();
  await openTask();
  const note = taskSection.getByLabel("Result note (optional)");
  await note.fill("Keep this note");
  await expand();
  await expand();
  await openTask();
  assert.equal(await note.inputValue(), "Keep this note");
  await page.getByRole("button", { name: "Toggle tasks", exact: true }).click();
  await page.getByRole("button", { name: "Toggle tasks", exact: true }).click();
  await expand();
  await openTask();
  assert.equal(await note.inputValue(), "Keep this note");
  await page
    .getByRole("button", { name: "Toggle compact", exact: true })
    .click();
  await taskSection.getByLabel("Search your tasks").fill("absent");
  assert.equal(await taskSection.locator("[data-user-task]").count(), 0);
  await taskSection.getByLabel("Search your tasks").fill("");
  await openTask();
  assert.equal(await note.inputValue(), "Keep this note");
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  assert.equal(await note.inputValue(), "");
  await note.fill("Separate note");
  await page
    .getByRole("button", { name: "Switch workspace", exact: true })
    .click();
  assert.equal(await note.inputValue(), "Keep this note");
  await page.locator('[data-complaint="complaint"]').click();
  const detail = page.getByRole("dialog", {
    name: "Message to you",
    exact: true,
  });
  await detail.getByRole("alert").waitFor();
  assert.equal(await detail.getByText("Loading…", { exact: true }).count(), 0);
  await detail.getByRole("button", { name: "Retry", exact: true }).click();
  await detail.getByText("Please help", { exact: true }).waitFor();
  assert.equal(await detail.getByRole("alert").count(), 0);
  const reply = detail.getByLabel("Reply", { exact: true });
  await reply.fill("Original text");
  await page.keyboard.press("Escape");
  await detail.waitFor({ state: "hidden" });
  await page.locator('[data-complaint="complaint"]').click();
  assert.equal(await reply.inputValue(), "Original text");
  await detail.getByRole("button", { name: "Send reply", exact: true }).click();
  await page.waitForFunction(
    () => document.querySelector(".complaint-reply textarea")?.disabled,
  );
  await waitForSent(1);
  assert.equal(await reply.isDisabled(), true);
  assert.equal(sent[0].text, "Original text");
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
  await page.waitForFunction(
    () => document.querySelector(".complaint-reply textarea")?.disabled,
  );
  await waitForSent(2);
  releaseSend();
  await detail.locator(".complaint-response").getByText("New reply").waitFor();
  assert.equal(sent[1].text, "New reply");
  assert.equal(sent[0].action, "respond");
  assert.equal(sent[0].complaint_id, "complaint");
  assert.equal(sent[0].version, 1);
  assert.equal(sent[1].version, 2);
  assert.notEqual(sent[0].id, sent[1].id);
  assert.deepEqual(pageErrors, []);
  console.log(
    JSON.stringify({
      ok: true,
      checks: [
        "same-folder retry",
        "task note collapse, remount, filter, workspace isolation",
        "message detail error and retry",
        "reply draft retained after reopen",
        "reply fixed during send, subsequent response uses new identity and version",
      ],
    }),
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
