import { test } from "../playwright.mjs";
// Real UI components with isolated HTTP responses. No model or user state.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Ux request recovery browser", async ({
  browser: testBrowser,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const cache = await mkdtemp(join(tmpdir(), "studio-ux-request-recovery-"));
  const entry = join(root, "workspaces/client/apps/web/__request-recovery.jsx");
  const source = `
  import React from 'react';
  import {createRoot} from 'react-dom/client';
  import {MantineProvider} from '@mantine/core';
  import '@mantine/core/styles.css';
  import Picker from '/src/components/ProjectDirectoryPicker.tsx';
  import {ClaudeSettings} from '/src/components/ClaudeSettings.tsx';
  import {ProjectNameForm,MoveChatForm} from '/src/components/ProjectOrganization.tsx';
  import ProjectAccount from '/src/components/ProjectAccount.tsx';
  import {useAccounts,AccountTransferStatus} from '/src/components/Accounts.tsx';
  const mode=new URLSearchParams(location.search).get('case');
  const project={id:'/project',path:'/project',name:'Project',accountKey:'ready',accountKeys:['ready','expired'],folders:[{id:'review',name:'Review'}]};
  const agent={id:'lead',rootId:'lead',name:'Lead',provider:'claude',accountKey:'ready',status:'idle',model:'claude-opus-4-6',projectFolder:null,projectFolderRevision:0};
  const accounts={accounts:[{id:'ready',label:'Ready account',status:'ready'},{id:'expired',label:'Expired account',status:'signedOut'}],defaultAccountKey:'ready'};
  window.selected=[];window.saves=0;window.failRefresh=mode==='confirmed';
  const saved=async()=>{window.saves++;if(window.failRefresh){window.failRefresh=false;throw Error('Snapshot unavailable');}};
  function AccountsFixture(){const state=useAccounts('/fixture');return <><button onClick={()=>state.refresh()}>Refresh accounts</button><button onClick={()=>state.setData({accounts:[{id:'ready',label:'New account',status:'ready'}],defaultAccountKey:'ready'})}>Use saved accounts</button><output>{state.data.accounts.map(a=>a.label).join(',')}</output></>;}
  let body=mode==='picker'?<Picker initialPath='/project' onSelect={async path=>window.selected.push(path)}/>:
  mode==='claude'||mode==='deadline'?<ClaudeSettings agent={agent}/>:
  mode.startsWith('accounts')?<AccountsFixture/>:
  ['name','confirmed','rejected','project-deadline'].includes(mode)?<ProjectNameForm project={project} folder='new' saved={saved}/>:
  mode==='move'?<MoveChatForm project={project} agent={agent} saved={saved}/>:
  mode==='project-account'||mode==='unready'?<ProjectAccount path={project.path} project={mode==='unready'?{...project,accountKey:'expired'}:project} defaultAccountKey='ready' accounts={accounts} saved={saved}/>:
  <AccountTransferStatus transfer={{status:'pending',moved:1,total:1,waitingCount:0,nativeHistoryPending:1}} targetLabel='Ready account' onAction={()=>{}}/>;
  createRoot(document.getElementById('root')).render(<MantineProvider>{body}</MantineProvider>);
  `;
  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(root, "web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "request-recovery-fixture",
        resolveId(id) {
          if (id === "/__request-recovery.jsx") return entry;
        },
        load(id) {
          if (id === entry) return source;
        },
        configureServer(server) {
          server.middlewares.use((req, res, next) => {
            if (new URL(req.url, "http://fixture").pathname !== "/")
              return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/__request-recovery.jsx"></script>',
            );
          });
        },
      },
    ],
  });
  const results = [];
  const selectedCases = [];
  try {
    await server.listen();
    for (const mode of selectedCases.length
      ? selectedCases
      : [
          "picker",
          "claude",
          "deadline",
          "accounts",
          "accounts-write",
          "name",
          "move",
          "project-account",
          "confirmed",
          "rejected",
          "project-deadline",
          "unready",
          "transfer",
        ]) {
      const page = await runnerContext.newPage();
      await page.setViewportSize({ width: 390, height: 844 });
      page.setDefaultTimeout(3000);
      const writes = [],
        errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      let reads = 0,
        releaseOld;
      if (mode.endsWith("deadline"))
        await page.addInitScript(() => {
          const original = window.setTimeout;
          window.setTimeout = (callback, delay, ...args) =>
            original(callback, delay === 15000 ? 100 : delay, ...args);
        });
      await page.route("**/api/**", async (route) => {
        const request = route.request(),
          url = new URL(request.url()),
          path = url.pathname;
        if (path === "/api/models")
          return route.fulfill({ json: { data: [] } });
        if (path === "/api/directories") {
          const typed = url.searchParams.get("path") || "/project";
          return route.fulfill({
            json: {
              path: typed === "~/work" ? "/home/user/work" : typed,
              directories: [],
            },
          });
        }
        if (path === "/api/claude/session") {
          reads++;
          if (mode === "deadline") return;
          if (reads === 1)
            return route.fulfill({
              status: 503,
              json: { error: "Settings unavailable" },
            });
          return route.fulfill({
            json: {
              version: "fixture",
              settings: { permissionMode: "default", thinking: true },
              turns: [],
            },
          });
        }
        if (path === "/api/accounts") {
          const sequence = ++reads;
          if (sequence === 1)
            await new Promise((resolve) => {
              releaseOld = resolve;
            });
          return route.fulfill({
            json: {
              accounts: [
                {
                  id: "ready",
                  label: sequence === 1 ? "Old account" : "New account",
                  status: "ready",
                },
              ],
              defaultAccountKey: "ready",
            },
          });
        }
        writes.push(request.postDataJSON());
        if (mode === "project-deadline" && writes.length === 1) return;
        if (mode === "rejected" && writes.length === 1)
          return route.fulfill({
            status: 400,
            json: { error: "Name already exists" },
          });
        if (mode !== "confirmed" && writes.length === 1)
          return route.fulfill({
            status: 503,
            json: { error: "Response lost after commit" },
          });
        return route.fulfill({ json: { projectFolder: "review" } });
      });
      try {
        await page.goto(`${server.resolvedUrls.local[0]}?case=${mode}`);
        if (mode === "picker") {
          const use = page.getByRole("button", {
            name: "Use this folder",
            exact: true,
          });
          await page.waitForFunction(
            () => !document.querySelector(".directory-footer button")?.disabled,
          );
          await page.getByLabel("Folder path").fill("/other");
          assert.equal(
            await use.isEnabled(),
            false,
            "A typed path must not select the old directory",
          );
          await page.getByRole("button", { name: "Go", exact: true }).click();
          await page.waitForFunction(
            () => !document.querySelector(".directory-footer button")?.disabled,
          );
          await use.click();
          assert.deepEqual(await page.evaluate(() => window.selected), [
            "/other",
          ]);
          await page.getByLabel("Folder path").fill("~/work");
          await page.getByRole("button", { name: "Go", exact: true }).click();
          await page.waitForFunction(
            () =>
              document.querySelector(".directory-location")?.textContent ===
              "/home/user/work",
          );
          assert.equal(
            await page.getByLabel("Folder path").inputValue(),
            "/home/user/work",
          );
        } else if (mode === "claude") {
          await page
            .getByRole("alert")
            .filter({ hasText: "Settings unavailable" })
            .waitFor();
          assert.equal(
            await page
              .getByRole("region", { name: "Claude settings" })
              .getAttribute("aria-busy"),
            "false",
          );
          assert.equal(
            await page
              .getByRole("button", {
                name: "Retry Claude settings",
                exact: true,
              })
              .count(),
            1,
          );
          await page
            .getByRole("button", { name: "Retry Claude settings", exact: true })
            .click();
          await page.waitForFunction(
            () => !document.querySelector("select")?.disabled,
          );
          assert.equal(await page.getByRole("alert").count(), 0);
        } else if (mode === "deadline") {
          await page
            .getByRole("alert")
            .filter({ hasText: "The server did not respond in time" })
            .waitFor();
          assert.equal(
            await page
              .getByRole("button", {
                name: "Retry Claude settings",
                exact: true,
              })
              .count(),
            1,
          );
        } else if (mode.startsWith("accounts")) {
          await page
            .getByRole("button", { name: "Refresh accounts", exact: true })
            .waitFor();
          await page.waitForFunction(() => !!document.querySelector("output"));
          for (let attempt = 0; attempt < 100 && !releaseOld; attempt++)
            await new Promise((resolve) => setTimeout(resolve, 10));
          assert.equal(typeof releaseOld, "function");
          await page
            .getByRole("button", {
              name:
                mode === "accounts-write"
                  ? "Use saved accounts"
                  : "Refresh accounts",
              exact: true,
            })
            .click();
          await page.getByText("New account", { exact: true }).waitFor();
          const oldResponse = page.waitForResponse((response) =>
            response.url().endsWith("/api/accounts"),
          );
          releaseOld();
          await oldResponse;
          await page.evaluate(
            () =>
              new Promise((resolve) =>
                requestAnimationFrame(() => requestAnimationFrame(resolve)),
              ),
          );
          assert.equal(
            await page.locator("output").textContent(),
            "New account",
            "A late response must not replace a newer account list",
          );
        } else if (mode === "transfer") {
          const text = await page.locator('[role="status"]').innerText();
          assert.equal(text.includes("0 waiting"), false);
          assert.match(text, /History transfer in progress: 1 remaining/);
        } else if (mode === "unready") {
          assert.equal(
            await page
              .getByRole("button", { name: "Save accounts", exact: true })
              .isEnabled(),
            false,
          );
          await page
            .getByLabel("Default account for new chats")
            .selectOption("ready");
          assert.equal(
            await page
              .getByRole("button", { name: "Save accounts", exact: true })
              .isEnabled(),
            true,
          );
        } else {
          const name = [
            "name",
            "confirmed",
            "rejected",
            "project-deadline",
          ].includes(mode);
          if (name) await page.getByLabel("Folder name").fill("Review");
          if (mode === "move")
            await page.getByLabel("Move to folder").selectOption("review");
          await page
            .getByRole("button", {
              name: name
                ? "Create folder"
                : mode === "move"
                  ? "Move chat"
                  : "Save accounts",
              exact: true,
            })
            .click();
          await page.getByRole("alert").waitFor();
          if (mode === "rejected") {
            assert.equal(
              await page.getByLabel("Folder name").isEnabled(),
              true,
            );
            await page.getByLabel("Folder name").fill("Different name");
            await page
              .getByRole("button", { name: "Create folder", exact: true })
              .click();
            await page.waitForFunction(() => window.saves === 1);
            assert.equal(writes.length, 2);
            assert.equal(writes[0].folder_id, writes[1].folder_id);
            assert.equal(writes[1].name, "Different name");
            assert.deepEqual(errors, []);
            results.push({ mode, passed: true });
            continue;
          }
          assert.equal(
            await page
              .getByLabel(
                name
                  ? "Folder name"
                  : mode === "move"
                    ? "Move to folder"
                    : "Default account for new chats",
              )
              .isEnabled(),
            false,
            "Unconfirmed requests must retain their exact data",
          );
          await page
            .getByRole("button", {
              name:
                mode === "confirmed"
                  ? "Refresh project"
                  : "Retry saved request",
              exact: true,
            })
            .click();
          await page.waitForFunction(
            () =>
              window.saves ===
              (window.location.search.includes("confirmed") ? 2 : 1),
          );
          assert.equal(writes.length, mode === "confirmed" ? 1 : 2);
          if (writes.length === 2) assert.deepEqual(writes[0], writes[1]);
        }
        assert.deepEqual(errors, []);
        results.push({ mode, passed: true });
      } catch (error) {
        await page.screenshot({ path: join(cache, `${mode}-failure.png`) });
        results.push({
          mode,
          passed: false,
          error: String(error.message).slice(0, 400),
        });
      } finally {
        releaseOld?.();
        await page.close();
      }
    }
    console.log(JSON.stringify({ results, evidence: cache }));
    assert.equal(results.filter((result) => !result.passed).length, 0);
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    if (results.every((result) => result.passed))
      await rm(cache, { recursive: true, force: true });
  }
});
