import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-radio-create-"));
const entry = join(root, "radio-create-fixture.tsx");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "fixture",
      configureServer(s) {
        s.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<html><body><div id="root"></div><script type="module" src="/radio-create-fixture.tsx"></script></body></html>',
          );
        });
      },
      resolveId(id) {
        if (id === "/radio-create-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `
import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider,Modal} from '@mantine/core';import '@mantine/core/styles.css';import SharedChatCreate from '/src/components/SharedChatCreate.tsx';import Sidebar from '/src/components/Sidebar.tsx';import '/src/style.css';import '/src/appearance.css';import '/src/workspace-layout.css';import {theme} from '/src/theme.ts';
const accounts={defaultAccountKey:'codex',accounts:[{id:'codex',label:'Personal',email:'personal@example.com',provider:'codex',status:'ready'},{id:'claude',label:'Work',email:'work@example.com',provider:'claude',status:'ready'}]};
const initial={stateDir:'create-test',threads:[],runtime:{projects:[{id:'p',path:'/p',name:'Project'}],rooms:[],requests:[],peerTeamsVersion:1,peerTeams:[]}};
function Fixture(){const[data,setData]=useState(initial);const[show,setShow]=useState({path:'/p'});const[opened,open]=useState(null);window.fixture=data;window.opened=opened;window.reopen=()=>setShow({path:'/p'});
const refresh=async()=>{if(window.failRefresh)throw Error('Snapshot unavailable');const result=await fetch('/snapshot');setData(await result.json());};
return <MantineProvider theme={theme} defaultColorScheme="dark"><Sidebar data={data} opened={opened} open={open} newChat={()=>window.ordinary=true} newSharedChat={path=>setShow({path})} addProject={()=>{}} changeProject={()=>{}} projectAccount={()=>{}} creating={false} rename={async()=>{}} remove={()=>{}} mobile={false} onSearch={()=>{}} close={()=>{}} refresh={refresh} indicators={new Map()} markUnread={()=>{}} markingRead={new Set()}/><Modal opened={!!show} title="New shared chat" onClose={()=>setShow(null)} size="md">{show && <SharedChatCreate data={data} accounts={accounts} initialPath={show.path} refresh={refresh} created={id=>{open(id);setShow(null);}}/>}</Modal></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
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
    viewport: { width: 1100, height: 950 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  let state,
    mode = "lost";
  const requests = [];
  const identities = new Map();
  await page.route("**/api/models?*", (r) =>
    r.fulfill({
      json: {
        data: r.request().url().includes("claude")
          ? [
              {
                model: "opus",
                displayName: "Claude · Opus",
                description: "Opus 5.5 · Native model",
                isDefault: true,
                supportedReasoningEfforts: [{ reasoningEffort: "high" }],
              },
            ]
          : [
              { model: "gpt-6-astra", displayName: "Astra", isDefault: true },
              { model: "gpt-6-luna", displayName: "Luna" },
            ],
      },
    }),
  );
  await page.route("**/snapshot", (r) => r.fulfill({ json: state }));
  await page.route("**/api/peer-teams", async (r) => {
    const body = r.request().postDataJSON();
    requests.push(body);
    if (!identities.has(body.request_id))
      identities.set(
        body.request_id,
        identities.size ? "radio:second" : "radio:direct",
      );
    const roomId = identities.get(body.request_id);
    assert.equal(body.radio_action, "create");
    assert.equal(body.participants.length, 2);
    state = {
      ...state,
      threads: body.participants.map((p, i) => ({
        id: "agent" + i,
        name: "Internal " + i,
        cwd: "/p",
        source: "managed",
        isLead: true,
        sharedRoomId: roomId,
        ...p,
      })),
      runtime: {
        ...state.runtime,
        rooms: [
          {
            id: roomId,
            name: body.name || "Shared chat",
            projectPath: "/p",
            kind: "private",
            members: ["agent0", "agent1"],
            radio: {
              direct: true,
              teamId: "direct",
              revision: 0,
              status: "idle",
              speaker: null,
              next: [],
              active: null,
              error: null,
            },
          },
        ],
      },
    };
    if (mode === "lost") {
      mode = "ok";
      return r.abort("failed");
    }
    return r.fulfill({ json: { room: state.runtime.rooms[0] } });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.waitForFunction(() => window.fixture);
  state = await page.evaluate(() => window.fixture);
  const form = page.getByRole("form", { name: "Create shared chat" });
  const create = form.getByRole("button", {
    name: "Create shared chat",
    exact: true,
  });
  await page.waitForFunction(
    () =>
      document.querySelector("form button[type=submit]")?.disabled === false,
  );
  assert.equal(
    await form.getByLabel("Model for agent 1", { exact: true }).inputValue(),
    "gpt-6-astra",
  );
  assert.equal(
    await form.getByLabel("Model for agent 2", { exact: true }).inputValue(),
    "opus",
  );
  assert.match(
    await form.getByLabel("Model for agent 2", { exact: true }).innerText(),
    /Opus 5.5/,
  );
  await form.getByLabel("Chat name").fill("Architecture discussion");
  for (const width of [1100, 375]) {
    await page.setViewportSize({ width, height: 950 });
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
      true,
    );
    await page.screenshot({
      path: join(tmpdir(), `studio-radio-create-${width}.png`),
      fullPage: true,
    });
  }
  await page.setViewportSize({ width: 1100, height: 950 });
  await create.click();
  await page.getByRole("button", { name: "Retry creation" }).waitFor();
  assert.equal(await page.evaluate(() => window.opened), null);
  assert.equal(await form.getByLabel("Chat name").isDisabled(), true);
  await page.reload();
  await page.getByRole("button", { name: "Retry creation" }).click();
  await page.waitForFunction(() => window.opened === "radio:direct");
  assert.equal(requests.length, 2);
  assert.deepEqual(requests[0], requests[1]);
  assert.equal(await page.locator(".sidebar-row").count(), 1);
  assert.equal(await page.locator(".peer-team").count(), 0);
  assert.equal(await page.getByText("Internal 0", { exact: true }).count(), 0);
  assert.equal(
    await page.locator(".sidebar-row").innerText(),
    "Architecture discussion",
  );
  await page.getByRole("button", { name: "New chat", exact: true }).click();
  assert.equal(await page.evaluate(() => window.ordinary), true);
  await page
    .getByRole("button", { name: "New shared chat", exact: true })
    .click();
  await page.waitForFunction(
    () =>
      document.querySelector("form button[type=submit]")?.disabled === false,
  );
  await page.evaluate(() => (window.failRefresh = true));
  await create.click();
  await page.getByRole("button", { name: "Refresh", exact: true }).waitFor();
  const posted = requests.length;
  await page.evaluate(() => (window.failRefresh = false));
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await form.waitFor({ state: "detached" });
  assert.equal(requests.length, posted);
  await page
    .getByRole("button", { name: "Options for project Project" })
    .click();
  await page.getByRole("menuitem", { name: "New shared chat" }).click();
  assert.equal(
    await form.getByLabel("Project", { exact: true }).inputValue(),
    "/p",
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS direct shared creation, two account/model selections, exact retry after reload, acknowledged refresh, one sidebar row, ordinary chat and project entry. Screenshots ${tmpdir()}/studio-radio-create-{1100,375}.png`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
