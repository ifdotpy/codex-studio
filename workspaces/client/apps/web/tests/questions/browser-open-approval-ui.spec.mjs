// A VM agent's sign-in link (browser/open, docs/vm-browser-bridge.md) renders as an approval
// card. Nothing opens before the approval; after it, the desktop opens only the link that the
// server returned. A refusal from the server, such as a remote window, opens nothing.
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.mjs";

const SIGN_IN =
  "https://accounts.example.com/oauth/authorize?client_id=kube&redirect_uri=http%3A%2F%2Flocalhost%3A8000&state=s";
const REMOTE =
  "Open this chat in the window of the Mac that runs its server to sign in, or use the tool's device-code mode.";

test("Browser Open Approval Ui", async ({ page }) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-browser-open-"));
  const harness = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import Requests from '/src/components/questions/Requests.tsx';
const agent={id:'worker',rootId:'lead',name:'Cluster worker',epoch:0,accountKey:'default'};
const card=(id,url,host,redirect)=>({id,agent:'worker',method:'browser/open',status:'pending',params:{bridgeId:id,url,host,redirect,clientId:'kube',scope:'openid',linuxUser:'studio-a-0123'}});
const requests=[
  card('approve',${JSON.stringify(SIGN_IN)},'accounts.example.com',{host:'localhost',port:8000,path:'/'}),
  card('remote','https://oidc.eu-central-1.amazonaws.com/authorize?x=1','oidc.eu-central-1.amazonaws.com',{host:'127.0.0.1',port:40645,path:'/oauth/callback'}),
  card('device','https://device.sso.eu-central-1.amazonaws.com/?user_code=ABCD','device.sso.eu-central-1.amazonaws.com',null),
];
window.__notes=[];
createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,null,React.createElement(Requests,{requests,allRequests:requests,scope:'fixture',agents:[agent],refresh:async()=>{},notify:(s)=>window.__notes.push(s)})));
`;
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: join(temporary, "vite"),
    plugins: [
      {
        name: "browser-open-fixture",
        resolveId(id) {
          return id === "virtual:browser-open" ? "\0" + id : undefined;
        },
        load(id) {
          return id === "\0virtual:browser-open" ? harness : undefined;
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    page.setDefaultTimeout(5000);
    // The desktop bridge records what it would open; no real browser opens.
    await page.addInitScript(() => {
      window.__opened = [];
      window.codexDesktop = {
        platform: "test",
        openExternal: async (url) => window.__opened.push(["external", url]),
        openSignIn: async (url) => window.__opened.push(["signIn", url]),
      };
    });
    const answers = [];
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "POST")
        return route.fulfill({ json: {} });
      const body = route.request().postDataJSON();
      answers.push(body);
      if (body.id === "remote")
        return route.fulfill({ status: 400, json: { error: REMOTE } });
      if (body.decision === "accept")
        return route.fulfill({
          json: {
            status: "answered",
            openUrl: body.id === "approve" ? SIGN_IN : "https://x.invalid/",
          },
        });
      return route.fulfill({ json: { status: "answered" } });
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__virtual:browser-open";</script>',
      }),
    );
    await page.goto(server.resolvedUrls.local[0] + "check");
    const approve = page.locator('[data-request="approve"]');
    await approve.waitFor();
    await expect(approve.getByText("Cluster worker")).toBeVisible();
    await expect(
      approve.getByText("Open a sign-in page in your browser"),
    ).toBeVisible();
    await expect(
      approve.getByText("accounts.example.com", { exact: true }),
    ).toBeVisible();
    await expect(approve.getByText("kube", { exact: true })).toBeVisible();
    await expect(
      approve.getByText(
        "The sign-in result goes to this chat's Linux user (studio-a-0123) on port 8000 of this Mac.",
      ),
    ).toBeVisible();
    // The card offers no direct link: the link opens only through the approval.
    await expect(approve.locator("a[href]")).toHaveCount(0);
    await expect(
      page
        .locator('[data-request="device"]')
        .getByText("This page needs no forward."),
    ).toBeVisible();
    expect(await page.evaluate(() => window.__opened)).toEqual([]);

    await approve
      .getByRole("button", { name: "Open in browser", exact: true })
      .click();
    await expect
      .poll(() => page.evaluate(() => window.__opened))
      .toEqual([["signIn", SIGN_IN]]);

    const remote = page.locator('[data-request="remote"]');
    await remote
      .getByRole("button", { name: "Open in browser", exact: true })
      .click();
    await expect
      .poll(() => page.evaluate(() => window.__notes))
      .toContain(REMOTE);
    await expect(remote.getByRole("alert")).toBeVisible();

    await page
      .locator('[data-request="device"]')
      .getByRole("button", { name: "Decline", exact: true })
      .click();
    await expect.poll(() => answers.length).toBe(3);
    expect(answers).toEqual([
      { decision: "accept", id: "approve" },
      { decision: "accept", id: "remote" },
      { decision: "decline", id: "device" },
    ]);
    expect(await page.evaluate(() => window.__opened)).toEqual([
      ["signIn", SIGN_IN],
    ]);
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
