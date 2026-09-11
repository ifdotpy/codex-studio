#!/usr/bin/env node
// Current quota recovery and immutable history through real React components.
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
const cache = await mkdtemp(join(tmpdir(), "limit-recovery-state-vite-"));
const entry = join(repo, "web/__limit-recovery-fixture.jsx");
const now = Math.floor(Date.now() / 1000);
const source = `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import {NativeError, NativeNotice} from '/src/components/NativeNotice.tsx';
import Usage from '/src/components/Usage.tsx';
const error={codexErrorInfo:'usageLimitExceeded',message:'Historical quota failure'};
const agent={id:'lumina',rootId:'lumina',accountKey:'work',threadId:'native-lumina',model:'gpt-6-astra',
  error,nativeLimitErrorAt:${now - 10},status:'failed',isLead:true};
const item={id:'past-error',role:'system',nativeNotice:'error',nativeError:error,text:error.message};
window.fixtureActions=[];
const action=()=>window.fixtureActions.push('action');
function Fixture(){
  const [snapshot,setSnapshot]=useState(()=>JSON.parse(sessionStorage.getItem('fixture-initial')||'null')||{label:'initial',limits:null});
  window.applyLimitSnapshot=setSnapshot;
  const currentAgent={...agent,id:snapshot.agentId||agent.id,
    accountKey:snapshot.accountKey||agent.accountKey,threadId:snapshot.threadId||agent.threadId,
    turnId:snapshot.turnId,lastCompletedTurn:snapshot.lastCompletedTurn,
    nativeLimitErrorAt:snapshot.episode??agent.nativeLimitErrorAt};
  return <MantineProvider><main data-case={snapshot.label}>
    <section id='current'><NativeError key={snapshot.mountKey} agent={currentAgent} limits={snapshot.limits} openLimits={action}/></section>
    <section id='history'><NativeNotice item={item}/></section>
    <section style={{position:'fixed',bottom:20,right:20}}>
      <Usage key={snapshot.mountKey} agent={currentAgent} limits={snapshot.limits} reload={action} opened={true} onChange={action} limitsLoading={false}/>
    </section>
    <output id='saved-error'>{JSON.stringify(agent.error)}</output>
  </main></MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Fixture/>);`;
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: join(repo, "web"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "limit-recovery-fixture",
      resolveId(id) {
        if (id === "/__limit-recovery-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__limit-recovery-fixture.jsx"></script>',
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
    viewport: { width: 1200, height: 900 },
  });
  await page.addInitScript((clock) => {
    Date.now = () => clock * 1000;
  }, now);
  const errors = [],
    writes = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/**", async (route) => {
    if (route.request().method() !== "GET") writes.push(route.request().url());
    const path = new URL(route.request().url()).pathname;
    await route.fulfill({
      json:
        path === "/api/costs"
          ? { accountKey: "work", error: "Isolated cost fixture" }
          : {},
    });
  });
  await page.goto(server.resolvedUrls.local[0]);
  await page.locator(".account-limits-panel").waitFor();
  const originalError = await page.locator("#saved-error").textContent();
  const assertBanner = async (visible) => {
    await page.locator(".account-limits-panel").waitFor();
    await page.waitForFunction(
      (expected) =>
        !!document.querySelector("#current .native-error") === expected &&
        !!document.querySelector(
          ".account-limits-panel .account-limit-recovery",
        ) === expected,
      visible,
    );
    assert.equal(
      await page.locator("#history .native-limit-notice").count(),
      1,
    );
    assert.match(
      await page.locator("#history").textContent(),
      /Historical quota failure/,
    );
    assert.equal(
      await page.locator("#saved-error").textContent(),
      originalError,
    );
  };
  const apply = async (label, limits, visible, identity = {}) => {
    await page.evaluate((snapshot) => window.applyLimitSnapshot(snapshot), {
      label,
      limits,
      ...identity,
    });
    await page.locator(`main[data-case="${label}"]`).waitFor();
    await assertBanner(visible);
  };
  await assertBanner(true);
  const available = {
    accountKey: "work",
    at: now,
    data: {
      ordinaryUsageAllowed: true,
      rateLimits: {
        planType: "pro",
        rateLimitReachedType: null,
        primary: { usedPercent: 14, resetsAt: now + 3600 },
        secondary: { usedPercent: 35, resetsAt: now + 86400 },
      },
    },
  };
  const cases = [
    ["another-account", { ...available, accountKey: "another" }],
    ["stale", { ...available, stale: true }],
    ["expired", { ...available, at: now - 301 }],
    ["older-than-error", { ...available, at: now - 20 }],
    ["same-time-as-error", { ...available, at: now - 10 }],
    ["future", { ...available, at: now + 60 }],
    ["loading", { ...available, loading: true }],
    ["read-error", { ...available, error: "Offline" }],
    [
      "unknown",
      { ...available, data: { rateLimits: available.data.rateLimits } },
    ],
    ["no-snapshot", null],
    [
      "usage-denied",
      {
        ...available,
        data: { ...available.data, ordinaryUsageAllowed: false },
      },
    ],
    [
      "exhausted",
      {
        ...available,
        data: {
          ...available.data,
          rateLimits: {
            primary: { usedPercent: 100, resetsAt: now + 3600 },
          },
        },
      },
    ],
  ];
  for (const [label, limits] of cases) {
    await apply(label, limits, true, { agentId: label });
    await apply(label + "-recovered", available, false, { agentId: label });
  }
  await apply("recovered-episode", available, false);
  await apply("recovered-now-stale", { ...available, stale: true }, false);
  await apply(
    "recovered-read-error",
    { ...available, error: "Offline" },
    false,
  );
  await apply("recovered-cache-aged", { ...available, at: now - 301 }, false);
  const stale = { ...available, stale: true };
  await apply("same-episode-new-turn-identity", stale, false, {
    turnId: "later-turn",
    lastCompletedTurn: "completed-turn",
  });
  await apply("recovered-component-remount", stale, false, { mountKey: 1 });
  await apply("other-account-after-recovery", stale, true, {
    accountKey: "another",
    mountKey: 2,
  });
  await apply("other-thread-after-recovery", stale, true, {
    threadId: "another-thread",
    mountKey: 3,
  });
  assert.deepEqual(await page.evaluate(() => window.fixtureActions), []);
  await page.evaluate(
    (snapshot) => {
      sessionStorage.setItem("fixture-initial", JSON.stringify(snapshot));
    },
    { label: "recovered-page-reload", limits: stale },
  );
  await page.reload();
  await page.locator('main[data-case="recovered-page-reload"]').waitFor();
  await assertBanner(false);
  await apply("new-quota-error", available, true, { episode: now });
  assert.deepEqual(writes, []);
  assert.deepEqual(await page.evaluate(() => window.fixtureActions), []);
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      retainedCases: cases.length,
      currentBannerClearsWithoutSend: true,
      usageNoticeClears: true,
      historicalErrorPreserved: true,
      recoveredEpisodeStaysClear: true,
      remountAndReloadKeepRecovery: true,
      sameEpisodeTurnChangesKeepRecovery: true,
      savedRecoveryAccountAndThreadScoped: true,
      newQuotaErrorVisible: true,
    }),
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
