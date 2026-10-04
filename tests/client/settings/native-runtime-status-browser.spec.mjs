import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "../playwright.mjs";

test("Native runtime status", async ({ context }) => {
  test.setTimeout(180_000);

  const root = fileURLToPath(new URL("../../../web/", import.meta.url));
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-native-runtime-ui-"));
  const entry = join(root, "native-runtime-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "native-runtime-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<html><body><div id="root"></div><script type="module" src="/native-runtime-fixture.tsx"></script></body></html>',
            );
          });
        },
        resolveId(id) {
          if (id === "/native-runtime-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';
  import NativeRuntimeStatus from '/src/components/NativeRuntimeStatus.tsx';import Accounts from '/src/components/Accounts.tsx';import '/src/studio-theme.css';import '/src/appearance.css';
  import ConversationWarnings from '/src/components/ConversationWarnings.tsx';
  const accounts=[{id:'default',label:'Personal',status:'ready'},{id:'work',label:'Work with a very long account label for narrow screens',status:'ready'},{id:'claude',label:'Claude profile',provider:'claude',status:'ready'}];
  const notices=[{id:'codex-warning',accountKey:'default',message:'Codex account version warning'},{id:'claude-warning',accountKey:'claude',message:'Claude account version warning'}];
  function Fixture(){const [opened,setOpened]=useState(false);const [integrated,setIntegrated]=useState(false);const [warningAccount,setWarningAccount]=useState('default');window.setRuntimeOpen=setOpened;window.setWarningAccount=setWarningAccount;window.showAccounts=()=>setIntegrated(true);
  return <MantineProvider><main style={{maxWidth:600,padding:12}}>{integrated?<Accounts state={{data:{accounts,defaultAccountKey:'default'},setData:()=>{},refresh:async()=>{},error:'',scope:'fixture'}} accountKey='default' changeAccount={async()=>{}} onError={()=>{}}/>:<NativeRuntimeStatus opened={opened} accounts={accounts}/>}<ConversationWarnings scope='version-warning' notices={notices} accountKey={warningAccount} messages={[]}/></main></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  const initial = {
    status: "ready",
    checkedAt: 1790092579,
    selected: {
      version: "0.116.0",
      sourcePath: "/private/very/long/path/codex",
      sha256: "fixture",
    },
    candidates: [
      {
        version: "0.117.0",
        path: "/private/rejected/codex",
        status: "rejected",
        error: "Required method thread/resume is missing",
      },
      {
        version: "0.116.0",
        path: "/private/approved/codex",
        status: "approved",
      },
    ],
    accounts: {
      default: { status: "current", version: "0.116.0" },
      work: {
        status: "waiting",
        version: "0.115.0",
        targetVersion: "0.116.0",
        reason: "Waiting for active turns to finish",
      },
    },
  };
  try {
    await server.listen();
    const page = await context.newPage();
    await page.setViewportSize({ width: 1100, height: 850 });
    const errors = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      console.error(error);
    });
    let reads = 0;
    const providerWarning = {
      id: "provider-version:default",
      accountKey: "default",
      provider: "codex",
      version: "0.153.3",
      baseline: "0.153.4",
      message:
        "Codex CLI 0.153.3 is older than this repository's lowest recorded tested version (0.153.4). It may work poorly or fail. You can continue at your own risk.",
    };
    const currentProviderStatus = {
      id: "provider-version:default",
      accountKey: "default",
      provider: "codex",
      status: "outdated",
      runningVersion: "0.153.3",
      installedVersion: "0.153.3",
      configuredVersion: null,
      baseline: "0.153.4",
      message: providerWarning.message,
    };
    let body = {
      nativeRuntime: initial,
      providerVersions: {
        checkedAt: 1790092579,
        providers: [currentProviderStatus],
        warnings: [providerWarning],
      },
    };
    await page.route("**/api/**", (route) => {
      assert.equal(
        route.request().method(),
        "GET",
        "Status must stay read-only",
      );
      if (new URL(route.request().url()).pathname === "/api/desktop") {
        reads++;
        return route.fulfill({ json: body });
      }
      return route.fulfill({ json: { accounts: [], data: null } });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.waitForFunction(() => !!window.setRuntimeOpen);
    await page.clock.install();
    await page.clock.runFor(12000);
    assert.equal(reads, 0, "Closed panel must not read diagnostics");
    await page.evaluate(() => window.setRuntimeOpen(true));
    const panel = page.getByRole("region", { name: "Codex runtime" });
    const providerPanel = page.getByRole("region", {
      name: "Provider versions",
    });
    await panel.waitFor();
    await providerPanel.waitFor();
    assert.match(await panel.innerText(), /0\.116\.0/);
    assert.match(
      await providerPanel.innerText(),
      /lowest recorded tested version/,
    );
    assert.match(await providerPanel.innerText(), /continue at your own risk/);
    assert.match(await providerPanel.innerText(), /Running: 0\.153\.3/);
    assert.match(
      await providerPanel.innerText(),
      /Lowest repo-tested version: 0\.153\.4/,
    );
    assert.match(await panel.innerText(), /1 account pending/);
    assert.match(await panel.innerText(), /Personal/);
    assert.match(await panel.innerText(), /Waiting for active turns to finish/);
    assert.match(
      await panel.innerText(),
      /Version 0\.117\.0 rejected: Required method thread\/resume is missing/,
    );
    assert.equal(
      await panel.locator("time").getAttribute("datetime"),
      new Date(initial.checkedAt * 1000).toISOString(),
    );
    assert.equal(
      (await panel.innerText()).includes("/private/"),
      false,
      "Do not show verbose source paths",
    );
    assert.equal(await panel.getByRole("button").count(), 0);

    await page.getByRole("button", { name: "Warnings", exact: true }).click();
    const warningsDialog = page.getByRole("dialog", {
      name: "Warnings",
      exact: true,
    });
    await warningsDialog.getByText("Codex account version warning").waitFor();
    assert.equal(
      await warningsDialog.getByText("Claude account version warning").count(),
      0,
    );
    await page.keyboard.press("Escape");
    await page.evaluate(() => window.setWarningAccount("claude"));
    await page.getByRole("button", { name: "Warnings", exact: true }).click();
    await warningsDialog.getByText("Claude account version warning").waitFor();
    assert.equal(
      await warningsDialog.getByText("Codex account version warning").count(),
      0,
    );
    await page.keyboard.press("Escape");

    body = {
      nativeRuntime: {
        ...initial,
        accounts: { work: { status: "updating", targetVersion: "0.116.0" } },
      },
    };
    const readsBeforeRefresh = reads;
    await page.clock.runFor(10100);
    await page.waitForFunction(() =>
      document
        .querySelector(".native-runtime-accounts")
        ?.textContent.includes("Updating"),
    );
    assert.ok(reads > readsBeforeRefresh, "An open panel refreshes its status");
    body = {
      nativeRuntime: {
        ...initial,
        status: "failed",
        accounts: { work: { status: "failed", reason: "Handshake failed" } },
      },
    };
    await page.clock.runFor(10100);
    await page.waitForFunction(() =>
      document
        .querySelector(".native-runtime-status")
        ?.textContent.includes("Handshake failed"),
    );
    assert.match(await panel.innerText(), /Runtime check failed/);
    assert.match(await panel.innerText(), /Update failed/);
    body = {
      nativeRuntime: {
        ...initial,
        status: "checking",
        selected: null,
        checkedAt: 0,
        candidates: [],
        accounts: {},
      },
      providerVersions: { checkedAt: 1790092590, providers: [] },
    };
    await page.clock.runFor(10100);
    await page.waitForFunction(() =>
      document
        .querySelector(".native-runtime-status")
        ?.textContent.includes("Checking installed versions"),
    );
    assert.equal(await panel.locator("time").count(), 0);

    const claudeOnly = {
      id: "provider-version:claude",
      accountKey: "claude",
      provider: "claude",
      status: "unknown",
      runningVersion: null,
      installedVersion: "Claude Code 2.1.277",
      configuredVersion: "Claude Code 2.2.0",
      baseline: "2.1.278",
      message:
        "The exact version already running in Claude sessions is unknown.",
    };
    body = {
      providerVersions: {
        checkedAt: 1790092600,
        providers: [claudeOnly],
        warnings: [],
      },
    };
    await page.clock.runFor(10100);
    await panel.waitFor({ state: "detached" });
    await providerPanel.waitFor();
    assert.match(
      await providerPanel.innerText(),
      /Claude Code · Claude profile/,
    );
    assert.match(await providerPanel.innerText(), /Running: Unknown/);
    assert.match(await providerPanel.innerText(), /Claude Code 2\.1\.277/);
    assert.match(await providerPanel.innerText(), /Running version unknown/);
    body = { providerVersions: { checkedAt: 1790092610, providers: [] } };
    await page.clock.runFor(10100);
    await page.waitForFunction(() =>
      document
        .querySelector(".provider-version-status")
        ?.textContent.includes("No connected provider CLIs were found"),
    );
    await page.evaluate(() => window.setRuntimeOpen(false));
    const beforeClose = reads;
    await page.clock.runFor(30000);
    assert.equal(reads, beforeClose, "Close must stop polling");

    // Ignore abort in this request to prove that a late response cannot replace a new open's data.
    await page.evaluate(() => {
      const original = window.fetch;
      window.fetch = (input, init) => {
        const request = new Request(input, init);
        if (new URL(request.url).pathname === "/api/desktop") {
          window.fetch = original;
          window.delayedRuntimeSignal = request.signal;
          return new Promise((resolve) => {
            window.finishOldRuntimeRead = resolve;
          });
        }
        return original(input, init);
      };
      window.setRuntimeOpen(true);
    });
    await page.waitForFunction(() => !!window.finishOldRuntimeRead);
    await page.evaluate(() => window.setRuntimeOpen(false));
    await page.waitForFunction(() => window.delayedRuntimeSignal.aborted);
    body = { nativeRuntime: initial };
    await page.evaluate(() => window.setRuntimeOpen(true));
    await panel.waitFor();
    await page.evaluate(() =>
      window.finishOldRuntimeRead(
        new Response(JSON.stringify({ nativeRuntime: null }), {
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );
    await page.clock.runFor(100);
    assert.equal(
      await panel.count(),
      1,
      "Old response must not erase new state",
    );
    await page.setViewportSize({ width: 320, height: 720 });
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
      true,
      "Panel must fit mobile width",
    );
    assert.equal(
      await panel.evaluate((el) => el.scrollWidth <= el.clientWidth),
      true,
    );
    assert.equal(
      await providerPanel.evaluate((el) => el.scrollWidth <= el.clientWidth),
      true,
    );

    await page.evaluate(() => window.showAccounts());
    await page.getByTestId("account-picker").click();
    await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
    await page
      .getByRole("dialog", { name: "Accounts", exact: true })
      .getByRole("region", { name: "Codex runtime" })
      .waitFor();
    assert.equal(
      await panel.evaluate((el) => el.scrollWidth <= el.clientWidth),
      true,
      "Panel must also fit inside the mobile Accounts dialog",
    );
    if (process.env.SCREENSHOT)
      await panel.screenshot({ path: process.env.SCREENSHOT });
    assert.deepEqual(errors, []);
    console.log(
      "PASS native runtime status: lifecycle, cancellation, stale responses, refresh, failures, labels, mobile width, Accounts integration",
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
