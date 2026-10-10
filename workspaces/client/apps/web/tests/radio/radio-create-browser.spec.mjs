import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { modelValue } from "../../../../../runtime/apps/server/tests/model-picker.mjs";
import { chooseSetupValue } from "../../../../../runtime/apps/server/tests/setup-controls.mjs";
import {
  handleEntitySyncFixtureRequest,
  test,
  expect,
} from "../playwright.mjs";

test("radio create browser", async ({ page: runnerPage }) => {
  test.setTimeout(90000);
  const root = resolve(import.meta.dirname, "../../");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-radio-create-"));
  const entry = join(root, "radio-create-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: {
      host: "127.0.0.1",
      port: 0,
      watch: { ignored: ["**/test-results/**", "**/playwright-report/**"] },
    },
    plugins: [
      {
        name: "fixture",
        configureServer(s) {
          s.middlewares.use((req, res, next) => {
            if (
              handleEntitySyncFixtureRequest(req, res, {
                workspaceId: "1234567890abcdef1234567890abcdef",
                snapshot: {
                  stateDir: "create-test",
                  threads: [],
                  runtime: { rooms: [], projects: [], requests: [] },
                },
                onStreamReady: (notify) =>
                  notify([
                    { kind: "models" },
                    ...[
                      "codex",
                      "claude",
                      ...Array.from({ length: 5 }, (_, i) => "extra" + i),
                    ].map((accountKey) => ({ kind: "limits", accountKey })),
                  ]),
              })
            )
              return;
            next();
          });
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
  const accounts={defaultAccountKey:'codex',accounts:[{id:'codex',label:'Personal',email:'personal@example.com',provider:'codex',status:'ready'},{id:'claude',label:'Work',email:'work@example.com',provider:'claude',status:'ready'},...Array.from({length:5},(_,i)=>({id:'extra'+i,label:'Account '+(i+3),email:'account'+i+'@example.com',provider:i<3?'codex':'claude',status:'ready'}))]};
  const initial={stateDir:'create-test',threads:[],runtime:{projects:[{id:'p',path:'/p',name:'Project'}],rooms:[],requests:[],peerTeamsVersion:1,peerTeams:[]}};
  function Fixture(){const[data,setData]=useState(initial);const[show,setShow]=useState({path:'/p'});const[opened,open]=useState(null);window.fixture=data;window.opened=opened;window.reopen=()=>setShow({path:'/p'});
  const refresh=async()=>{if(window.failRefresh)throw Error('Snapshot unavailable');const result=await fetch('/snapshot');setData(await result.json());};
  return <MantineProvider theme={theme} defaultColorScheme="dark"><Sidebar data={data} opened={opened} open={open} newChat={()=>window.ordinary=true} newSharedChat={path=>setShow({path})} addProject={()=>{}} changeProject={()=>{}} projectAccount={()=>{}} creating={false} rename={async()=>{}} remove={()=>{}} mobile={false} onSearch={()=>{}} close={()=>{}} refresh={refresh} indicators={new Map()} markUnread={()=>{}} markingRead={new Set()}/><Modal opened={!!show} title="New shared chat" onClose={()=>setShow(null)} size="md">{show && <SharedChatCreate data={data} accounts={accounts} initialPath={show.path} refresh={refresh} created={id=>{open(id);setShow(null);}}/>}</Modal></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 1100, height: 950 });
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
    await page.route("**/api/limits?*", (r) =>
      r.fulfill({
        json: {
          accountKey: new URL(r.request().url()).searchParams.get(
            "account_key",
          ),
          at: Date.now() / 1000,
          data: {
            rateLimits: {
              primary: {
                usedPercent: 30,
                windowDurationMins: 300,
                resetsAt: Date.now() / 1000 + 3600,
              },
            },
          },
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
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.waitForFunction(() => window.fixture);
    state = await page.evaluate(() => window.fixture);
    const form = page.getByRole("form", { name: "Create shared chat" });
    const create = form.getByRole("button", {
      name: "Create",
      exact: true,
    });
    await page.waitForFunction(
      () =>
        document.querySelector("form button[type=submit]")?.disabled === false,
    );
    await form.getByRole("button", { name: "Settings for agent 1" }).click();
    assert.equal(
      await modelValue(page.getByLabel("Model for agent 1", { exact: true })),
      "gpt-6-astra",
    );
    await chooseSetupValue(
      page.getByRole("radiogroup", {
        name: "Account for agent 1 provider",
        exact: true,
      }),
      "claude",
    );
    await expect(
      page.getByLabel("Account for agent 1", { exact: true }),
    ).toHaveAttribute("data-value", "codex");
    await expect(page.locator(".setup-account")).toHaveCount(3);
    await chooseSetupValue(
      page.getByRole("radiogroup", {
        name: "Account for agent 1 provider",
        exact: true,
      }),
      "codex",
    );
    await page.keyboard.press("Escape");
    await form.getByRole("button", { name: "Settings for agent 2" }).click();
    assert.equal(
      await modelValue(page.getByLabel("Model for agent 2", { exact: true })),
      "opus",
    );
    assert.match(
      await page.getByLabel("Model for agent 2", { exact: true }).innerText(),
      /Opus 5.5/,
    );
    await page.keyboard.press("Escape");
    await form.getByLabel("Chat name").fill("Architecture discussion");
    const screenshots = join(tmpdir(), "refresh-shared-screenshots");
    await mkdir(screenshots, { recursive: true });
    for (const [profile, width, colorScheme] of [
      ["dark", 1100, "dark"],
      ["light", 1100, "light"],
      ["mobile", 390, "dark"],
    ]) {
      await page.setViewportSize({ width, height: 950 });
      await page.emulateMedia({ colorScheme });
      await page.evaluate((scheme) => {
        document.documentElement.setAttribute(
          "data-mantine-color-scheme",
          scheme,
        );
      }, colorScheme);
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
        true,
      );
      await page.screenshot({
        path: join(screenshots, `${profile}-shared.png`),
      });
      await form.getByRole("button", { name: "Settings for agent 1" }).click();
      await expect(
        page.getByLabel("Account for agent 1", { exact: true }),
      ).toBeVisible();
      await expect(page.locator(".shared-create-picker")).toBeInViewport({
        ratio: 1,
      });
      await expect(page.locator(".setup-account")).toHaveCount(4);
      await page.screenshot({
        path: join(screenshots, `${profile}-shared-picker.png`),
      });
      await page.keyboard.press("Escape");
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
    const { request_id, ...body } = requests[0];
    assert.match(request_id, /^[0-9a-f-]{36}$/);
    assert.deepEqual(body, {
      action: "radio",
      radio_action: "create",
      path: "/p",
      name: "Architecture discussion",
      participants: [
        { account_key: "codex", model: "gpt-6-astra" },
        { account_key: "claude", model: "opus" },
      ],
    });
    assert.equal(await page.locator(".sidebar-row").count(), 1);
    assert.equal(await page.locator(".peer-team").count(), 0);
    assert.equal(
      await page.getByText("Internal 0", { exact: true }).count(),
      0,
    );
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
    const projectOptions = page.getByRole("button", {
      name: "Options for project Project",
    });
    await projectOptions.locator("..").hover();
    await projectOptions.click();
    await page.getByRole("menuitem", { name: "New shared chat" }).click();
    await expect(create).toBeEnabled();
    await form.getByRole("button", { name: "Settings for agent 1" }).click();
    await page.locator('[data-account-key="extra0"]').click();
    await expect(
      page.getByLabel("Model for agent 1", { exact: true }),
    ).toHaveAttribute("data-value", "gpt-6-astra");
    await page.getByRole("option", { name: "Luna 6", exact: true }).click();
    await page.keyboard.press("Escape");
    await form.getByRole("button", { name: "Settings for agent 2" }).click();
    await chooseSetupValue(
      page.getByLabel("Reasoning for agent 2", { exact: true }),
      "high",
    );
    await page.keyboard.press("Escape");
    await create.click();
    await form.waitFor({ state: "detached" });
    assert.equal(requests.at(-1).path, "/p");
    assert.deepEqual(requests.at(-1).participants, [
      { account_key: "extra0", model: "gpt-6-luna" },
      { account_key: "claude", model: "opus", effort: "high" },
    ]);
    expect(errors).toEqual([]);
    console.log(
      `PASS direct shared creation, two account/model selections, exact retry after reload, acknowledged refresh, one sidebar row, ordinary chat and project entry. Screenshots ${screenshots}`,
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
