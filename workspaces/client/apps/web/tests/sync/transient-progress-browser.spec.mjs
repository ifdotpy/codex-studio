import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { test } from "../playwright.mjs";

for (const flow of ["vault", "auth"]) {
  test(`transient ${flow} progress has zero idle HTTP reads`, async ({
    browser,
  }) => {
    const root = resolve(import.meta.dirname, "../..");
    const require = createRequire(join(root, "package.json"));
    const { createServer } = await import(require.resolve("vite"));
    const cacheDir = await mkdtemp(join(tmpdir(), "transient-progress-"));
    const entry = join(root, "transient-progress-fixture.tsx");
    const server = await createServer({
      configFile: false,
      root,
      cacheDir,
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "transient-progress-fixture",
          configureServer(server) {
            server.middlewares.use("/check", (_req, res) => {
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<div id="root"></div><script type="module" src="/transient-progress-fixture.tsx"></script>',
              );
            });
          },
          resolveId(id) {
            if (id === "/transient-progress-fixture.tsx") return entry;
          },
          load(id) {
            if (id !== entry) return;
            return `import React from 'react';
            import {createRoot} from 'react-dom/client';
            import {MantineProvider} from '@mantine/core';
            import '@mantine/core/styles.css';
            import VaultSetup from '/src/components/sidebar/VaultSetup.tsx';
            import ClaudeSignIn from '/src/components/ClaudeSignIn.tsx';
            import {ServerServicesProvider} from '/src/servers/ServerServicesContext.tsx';
            window.reads={facts:0,auth:0}; window.selectors=[];
            const listeners=new Map();
            const resources={
              watchResourceChanges(resource,accept){const key=JSON.stringify(resource);listeners.set(key,accept);window.selectors.push(resource);return()=>listeners.delete(key)},
              watchResourceConnection(accept){accept('live');return()=>{}},
            };
            window.progress=(resource,value)=>listeners.get(JSON.stringify(resource))?.({epoch:'fixture',revision:1,progress:{resource,value}});
            const api={
              get:async()=>{window.reads.auth++;throw new Error('unexpected auth read')},
              saved:(key,fallback)=>key==='claude-sign-in:fixture:claude-one'?'login-one':fallback,
              save:()=>{}, stableRequestId:()=>'', clearStableRequestId:()=>{},
            };
            const backend={post:async()=>{window.reads.facts++;return {prefix:'project-one',keys:false,site:'https://desk.example',region:'eu-west-1',profiles:[]}}};
            const services={owner:{id:'local',origin:location.origin},api,resources,fileDialogs:{showVaultKeys:async()=>{},canShowVaultKeys:()=>true}};
            const flow=new URL(location.href).searchParams.get('flow');
            createRoot(document.getElementById('root')).render(<MantineProvider><ServerServicesProvider services={services}>
              {flow==='vault'?<VaultSetup entry={{vault:{projectId:'p1'}}} name='Project' backend={backend} onClose={()=>{}} onDone={()=>{}}/>:
              <ClaudeSignIn account={{id:'claude-one',label:'Claude',provider:'claude',status:'signedOut'}} scope='fixture' onClose={()=>{}} onReady={()=>{window.ready=true}}/>}
            </ServerServicesProvider></MantineProvider>);`;
          },
        },
      ],
    });
    const page = await browser.newPage();
    try {
      await server.listen();
      await page.clock.install();
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/check?flow=${flow}`,
      );
      if (flow === "vault") {
        await page
          .getByRole("button", { name: "Show the paper key in Terminal" })
          .click();
      }
      const before = await page.evaluate(() => ({ ...window.reads }));
      await page.clock.fastForward(10_000);
      assert.deepEqual(await page.evaluate(() => window.reads), before);
      assert.deepEqual(
        before,
        flow === "vault" ? { facts: 1, auth: 0 } : { facts: 0, auth: 0 },
      );
      const kind = flow === "vault" ? "vault-readiness" : "auth-progress";
      await page.waitForFunction(
        (kind) => window.selectors.some((resource) => resource.kind === kind),
        kind,
      );
      await page.evaluate((flow) => {
        if (flow === "vault")
          window.progress(
            { kind: "vault-readiness", prefix: "project-one" },
            true,
          );
        else
          window.progress(
            { kind: "auth-progress", requestId: "login-one" },
            {
              requestId: "login-one",
              accountKey: "claude-one",
              status: "ready",
            },
          );
      }, flow);
      if (flow === "vault")
        await page.getByText("The paper key of project-one is made.").waitFor();
      else await page.waitForFunction(() => window.ready === true);
      console.log(
        JSON.stringify({
          flow,
          idleSeconds: 10,
          idleReads: 0,
          initialReads: before,
        }),
      );
    } finally {
      await page.close();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}
