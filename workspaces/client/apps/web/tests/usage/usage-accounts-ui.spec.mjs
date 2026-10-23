import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  test,
  apiSchemaHandshakeSse,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
} from "../playwright.mjs";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("usage accounts ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(repo, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const cache = await mkdtemp(join(tmpdir(), "usage-accounts-vite-"));
  const entry = join(
    repo,
    "workspaces/client/apps/web/__usage-accounts-fixture.jsx",
  );
  const now = Math.floor(Date.now() / 1000);
  const workspaceId = "abcdef0123456789abcdef0123456789";
  const epoch = "usage-accounts-fixture";
  const streams = new Set();
  const reads = { costs: 0, sessionCost: 0 };
  let revision = 0;
  const writeResources = (stream, reason, resources = stream.resources) => {
    if (reason !== "initial") revision++;
    const event = {
      protocol: 3,
      workspaceId,
      epoch,
      revision,
      reason,
      resources,
    };
    assert.ok(
      Array.isArray(event.resources) && Number.isFinite(event.revision),
    );
    stream.response.write(
      `event: resources\ndata: ${JSON.stringify(event)}\n\n`,
    );
  };
  const writeHeartbeat = (stream) => {
    const event = { protocol: 3, workspaceId, epoch, revision };
    assert.ok(Number.isFinite(event.revision));
    stream.response.write(
      `event: heartbeat\ndata: ${JSON.stringify(event)}\n\n`,
    );
  };
  let resetPayload;
  const source = `
  import React,{useState} from 'react';
  import {createRoot} from 'react-dom/client';
  import {MantineProvider} from '@mantine/core';
  import '@mantine/core/styles.css';
  import Usage from '/src/components/Usage.tsx';
  import {accountLimitFixtures} from '/src/components/AccountLimits.fixtures.ts';
  import {theme} from '/src/theme.ts';
  const geometry=location.search.includes('geometry');
  if(geometry){await import('/src/style.css');await import('/src/design/state.css');}
  const own={id:'chat',rootId:'chat',accountKey:'own',source:'managed',model:'gpt-6-astra',status:'completed',isLead:true,created:1,contextUsage:{tokens:10,window:100}};
  const ownLimits={accountKey:'own',at:${now},data:{accountId:'own-account',rateLimits:{limitId:'codex',primary:{usedPercent:20,resetsAt:${now + 3600}}}}};
  const teamLimits={accountKey:'team',at:${now},data:{accountId:'team-account',rateLimits:{limitId:'codex',primary:{usedPercent:92,resetsAt:${now + 3600}}},rateLimitResetCredits:{availableCount:1,credits:[{id:'team-credit',title:'Team reset',status:'available',resetType:'codexRateLimits'}]}}};
  window.reloadCalls=[];
  function Fixture(){
   const [multi,setMulti]=useState(location.search.includes('multi'));
   const [teamReady,setTeamReady]=useState(false);
   const accounts=geometry?accountLimitFixtures:[{key:'own',label:'Own account',email:'own@example.test',accountId:'own-account',limits:ownLimits,loading:false,reload:(force=false)=>{window.reloadCalls.push(['own',force]);}},...(multi?[{key:'team',label:'Team account',email:'team@example.test',provider:'openai',accountId:'team-account',limits:teamReady?teamLimits:null,loading:false,reload:(force=false)=>{window.reloadCalls.push(['team',force]);if(!teamReady)setTeamReady(true);}},{key:'claude',label:'Claude work',email:'claude@example.test',provider:'claude',limits:{accountKey:'claude',at:${now},data:{rateLimits:{limitId:'claude',primary:{usedPercent:50,resetsAt:${now + 3600}}}}},loading:false,reload:(force=false)=>window.reloadCalls.push(['claude',force])},{key:'unnamed',label:'unnamed',provider:'codex',limits:{accountKey:'unnamed',at:${now},data:{rateLimits:{limitId:'codex',primary:{usedPercent:85,resetsAt:${now + 3600}}}}},loading:false,reload:(force=false)=>window.reloadCalls.push(['unnamed',force])}]:[])];
   return <MantineProvider theme={theme} forceColorScheme={new URLSearchParams(location.search).get('scheme') || undefined}><div style={geometry?{display:'flex',alignItems:'end',width:'100%',height:'100dvh',padding:'var(--mantine-spacing-sm)',containerType:'inline-size',containerName:'conversation-footer'}:undefined}><Usage agent={geometry?{...own,accountKey:'codex-main'}:own} stateDir='/fixture' limits={ownLimits} accountLabel='own@example.test' reload={()=>window.reloadCalls.push(['fallback',true])} opened={true} onChange={()=>{}} limitsLoading={false} accounts={accounts}/></div></MantineProvider>;
  }
  createRoot(document.getElementById('root')).render(<Fixture/>);`;
  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "usage-accounts-fixture",
        resolveId(id) {
          if (id === "/__usage-accounts-fixture.jsx") return entry;
        },
        load(id) {
          if (id === entry) return source;
        },
        configureServer(vite) {
          vite.middlewares.use((req, res, next) => {
            if (req.url?.startsWith("/api/"))
              res.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
            if (req.url?.startsWith("/api/sync/identity")) {
              res.setHeader("Content-Type", "application/json");
              res.end(JSON.stringify({ workspaceId }));
              return;
            }
            if (req.url?.startsWith("/api/sync/stream")) {
              const stream = {
                response: res,
                resources: JSON.parse(
                  new URL(req.url, "http://localhost").searchParams.get(
                    "resources",
                  ) || "[]",
                ),
              };
              res.writeHead(200, {
                "Content-Type": "text/event-stream",
                "Cache-Control": "no-cache",
                Connection: "keep-alive",
              });
              res.write(apiSchemaHandshakeSse());
              streams.add(stream);
              writeResources(stream, "initial");
              const heartbeat = setInterval(() => writeHeartbeat(stream), 1000);
              res.on("close", () => {
                clearInterval(heartbeat);
                streams.delete(stream);
              });
              return;
            }
            if (req.url?.startsWith("/api/")) {
              res.setHeader("Content-Type", "application/json");
              if (req.url.startsWith("/api/session-cost")) {
                reads.sessionCost++;
                res.end(
                  JSON.stringify({
                    pricingState: "ready",
                    totalUSD: 0,
                    rootId: "chat",
                    claudeHistoryIncomplete: true,
                  }),
                );
                return;
              }
              if (req.url.startsWith("/api/costs")) {
                reads.costs++;
                const key =
                  new URL(req.url, "http://localhost").searchParams.get(
                    "account_key",
                  ) || "default";
                res.end(
                  JSON.stringify({
                    accountKey: key,
                    data: { todayUSD: 0, last30DaysUSD: 0 },
                  }),
                );
                return;
              }
              if (req.url.startsWith("/api/limits/reset")) {
                let body = "";
                req.on("data", (part) => (body += part));
                req.on("end", () => {
                  resetPayload = JSON.parse(body);
                  res.end(JSON.stringify({ outcome: "reset" }));
                });
                return;
              }
            }
            if (req.url?.split("?")[0] !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/__usage-accounts-fixture.jsx"></script>',
            );
          });
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = _browser;
    const page = await browser.newPage({
      viewport: { width: 1100, height: 900 },
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(server.resolvedUrls.local[0]);
    // Notes stay in the tooltip; the footer shows only the estimate and a marker.
    await page
      .locator(".session-cost-summary")
      .filter({ hasText: "*" })
      .waitFor({ state: "attached" });
    assert.match(
      await page.locator(".session-cost-summary").getAttribute("title"),
      /Earlier Claude totals may be incomplete/,
    );
    assert.doesNotMatch(
      await page.locator(".session-cost-summary").innerText(),
      /incomplete|Unpriced/,
    );
    assert.ok(reads.costs >= 1, "the initial costs baseline performed a GET");
    assert.ok(
      reads.sessionCost >= 1,
      "the initial session-cost baseline performed a GET",
    );
    const costsBeforeChange = reads.costs;
    const sessionCostBeforeChange = reads.sessionCost;
    for (const stream of streams) {
      const costs = stream.resources.filter(
        (resource) => resource.kind === "costs",
      );
      if (costs.length) writeResources(stream, "change", costs);
    }
    const changeDeadline = Date.now() + 3000;
    while (reads.costs === costsBeforeChange && Date.now() < changeDeadline)
      await new Promise((resolve) => setTimeout(resolve, 20));
    assert.equal(
      reads.costs,
      costsBeforeChange + 1,
      "a typed costs notification performs exactly one targeted read",
    );
    await new Promise((resolve) => setTimeout(resolve, 3200));
    assert.equal(
      reads.costs,
      costsBeforeChange + 1,
      "typed cost changes and heartbeats do not cause duplicate or idle reads",
    );
    assert.equal(
      reads.sessionCost,
      sessionCostBeforeChange,
      "a costs notification does not refresh per-agent session cost",
    );
    const _toggle = page.getByRole("button", {
      name: "Account limits",
      exact: true,
    });
    await page
      .getByRole("region", { name: "Account limits details" })
      .waitFor();
    assert.equal(
      await page.getByRole("tab").count(),
      1,
      "one account has one detail tile",
    );
    const ownLimitsPanel = page.locator(".account-limits-panel");
    await ownLimitsPanel.waitFor();
    assert.match(await ownLimitsPanel.innerText(), /80%\s*left/);
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() =>
      window.reloadCalls.some(([key, force]) => key === "own" && force),
    );

    await page.goto(server.resolvedUrls.local[0] + "?multi");
    const ownTab = page.getByRole("tab", {
      name: "own@example.test account limits",
    });
    const teamTab = page.getByRole("tab", {
      name: "team@example.test, openai account limits",
    });
    await ownTab.waitFor();
    assert.equal(
      await ownTab.getAttribute("aria-selected"),
      "true",
      "chat account is selected first",
    );
    // The detail panel and the account tile state the same remaining limit.
    const accountPanel = page.locator("#usage-account-panel");
    const selectedOwnLimit = accountPanel.getByText("80% left", {
      exact: true,
    });
    await selectedOwnLimit.waitFor();
    assert.equal(await selectedOwnLimit.isVisible(), true);
    assert.equal(await selectedOwnLimit.textContent(), "80% left");
    assert.equal(
      await ownTab.locator(".account-remaining").textContent(),
      "80% left",
    );
    await ownTab.press("ArrowRight");
    assert.equal(
      await teamTab.getAttribute("aria-selected"),
      "true",
      "ArrowRight selects the next account tile",
    );
    await page.waitForFunction(() =>
      window.reloadCalls.some(([key, force]) => key === "team" && !force),
    );
    const selectedTeamLimit = accountPanel.getByText("8% left", {
      exact: false,
    });
    await selectedTeamLimit.waitFor();
    assert.equal(await selectedTeamLimit.isVisible(), true);
    const teamResetCredits = page.getByRole("region", {
      name: "Limit reset credits",
    });
    await teamResetCredits.waitFor();
    assert.match(await teamResetCredits.innerText(), /Team reset/);
    await page
      .getByRole("button", { name: "Account limits", exact: true })
      .waitFor();
    assert.equal(
      await page
        .getByRole("button", { name: "Account limits" })
        .getAttribute("title"),
      "Low allowance: team@example.test",
    );
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() =>
      window.reloadCalls.some(([key, force]) => key === "team" && force),
    );
    await page
      .getByRole("button", { name: "Apply reset", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Use one reset credit", exact: true })
      .click();
    const resetStatus = page.getByText("Reset applied.", { exact: true });
    await resetStatus.waitFor();
    assert.equal(await resetStatus.textContent(), "Reset applied.");
    const reset = resetPayload;
    assert.ok(reset, "reset request reached the fixture server");
    assert.equal(reset.account_id, "team-account");
    assert.equal(reset.credit_id, "team-credit");
    const ownTile = page.getByRole("tab", {
      name: "own@example.test account limits",
      exact: true,
    });
    assert.equal(
      await ownTile.getAttribute("aria-current"),
      "true",
      "detail selection keeps the current chat marker",
    );
    assert.equal(
      await ownTile.locator(".account-limits-tile-name").innerText(),
      "Own account",
    );
    const teamMeter = teamTab.getByRole("meter");
    assert.equal(await teamMeter.getAttribute("aria-valuenow"), "8");
    const { remainingLimitColor } = await server.ssrLoadModule(
      "/src/components/accountLimitColor.ts",
    );
    assert.equal(
      await teamMeter
        .locator("span")
        .evaluate((element) => element.style.background),
      remainingLimitColor(
        Number(await teamMeter.getAttribute("aria-valuenow")),
      ),
    );
    await page
      .getByRole("radio", { name: "Claude", exact: true })
      .locator("..")
      .locator("label")
      .click();
    const claudeTile = page.getByRole("tab", {
      name: "claude@example.test, claude account limits",
      exact: true,
    });
    await claudeTile.waitFor();
    assert.equal(
      await page.getByRole("tab").count(),
      1,
      "the provider shows only its own accounts",
    );
    assert.equal(await claudeTile.getAttribute("aria-selected"), "true");
    assert.equal(await claudeTile.getAttribute("aria-current"), null);
    const claudeMeter = claudeTile.getByRole("meter");
    assert.equal(await claudeMeter.getAttribute("aria-valuenow"), "50");
    assert.equal(
      await claudeMeter
        .locator("span")
        .evaluate((element) => element.style.background),
      remainingLimitColor(
        Number(await claudeMeter.getAttribute("aria-valuenow")),
      ),
    );
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() =>
      window.reloadCalls.some(([key, force]) => key === "claude" && force),
    );
    await page
      .getByRole("radio", { name: "Codex", exact: true })
      .locator("..")
      .locator("label")
      .click();
    await ownTile.waitFor();
    assert.equal(await ownTile.getAttribute("aria-current"), "true");
    assert.equal(await ownTile.getAttribute("aria-selected"), "true");
    assert.equal(
      await ownTile
        .getByRole("meter")
        .locator("span")
        .evaluate((element) => element.style.background),
      remainingLimitColor(
        Number(await ownTile.getByRole("meter").getAttribute("aria-valuenow")),
      ),
    );
    const unnamedTile = page.getByRole("tab", {
      name: "Account name unavailable, codex account limits",
      exact: true,
    });
    assert.equal(
      await unnamedTile.locator(".account-limits-tile-name").innerText(),
      "Account name unavailable",
    );
    assert.equal(
      await unnamedTile.getByRole("meter").getAttribute("aria-valuenow"),
      "15",
    );
    assert.equal(
      await unnamedTile
        .getByRole("meter")
        .locator("span")
        .evaluate((element) => element.style.background),
      remainingLimitColor(
        Number(
          await unnamedTile.getByRole("meter").getAttribute("aria-valuenow"),
        ),
      ),
    );
    await unnamedTile.click();
    assert.equal(await unnamedTile.getAttribute("aria-selected"), "true");
    assert.equal(await ownTile.getAttribute("aria-current"), "true");
    assert.equal(
      await page
        .locator(
          ".account-limits-detail-panel[data-active] .account-limits-selected-name strong",
        )
        .innerText(),
      "Account name unavailable",
    );

    // The five-account edge fixture reserves the taller mounted provider panel.
    for (const width of [390, 1440])
      for (const scheme of ["light", "dark"]) {
        await page.setViewportSize({ width, height: 844 });
        await page.goto(
          server.resolvedUrls.local[0] + `?geometry&scheme=${scheme}`,
        );
        assert.equal(
          await page.locator(".context-use").innerText(),
          "Context 10%",
        );
        const dots = await page
          .locator(".account-limits-dot-target .account-limits-dot")
          .evaluateAll((elements) =>
            elements.map((element) => ({
              width: element.getBoundingClientRect().width,
              height: element.getBoundingClientRect().height,
              radius: getComputedStyle(element).borderRadius,
              color: element.getAttribute("data-color"),
              background: getComputedStyle(element).backgroundColor,
            })),
          );
        const palette =
          scheme === "light"
            ? {
                green: "rgb(52, 134, 109)",
                yellow: "rgb(166, 124, 33)",
                red: "rgb(179, 81, 81)",
                gray: "rgb(139, 144, 154)",
              }
            : {
                green: "rgb(103, 173, 145)",
                yellow: "rgb(213, 173, 82)",
                red: "rgb(204, 119, 119)",
                gray: "rgb(133, 139, 150)",
              };
        assert.equal(dots.length, 5);
        for (const dot of dots) {
          assert.equal(dot.width, 6);
          assert.equal(dot.height, 6);
          assert.equal(dot.radius, "50%");
          assert.equal(dot.background, palette[dot.color]);
        }
        const popover = page.locator(".account-limits-popover");
        await page
          .getByRole("button", { name: "Apply reset", exact: true })
          .waitFor();
        await page.waitForFunction(
          () =>
            getComputedStyle(document.querySelector(".account-limits-popover"))
              .opacity === "1",
        );
        await page.evaluate(
          () =>
            new Promise((resolve) =>
              requestAnimationFrame(() => requestAnimationFrame(resolve)),
            ),
        );
        assert.doesNotMatch(await popover.innerText(), /Session estimate/);
        assert.match(await popover.innerText(), /Estimated API cost/);
        const before = await popover.boundingBox();
        assert.equal(await page.getByRole("tab").count(), 3);
        const assertFits = async () => {
          assert.deepEqual(
            await popover.evaluate((element) => {
              const panel = element.querySelector(".account-limits-panel");
              return {
                clipped: panel.scrollHeight > panel.clientHeight + 1,
                scrollbars: [...element.querySelectorAll("*")]
                  .filter(
                    (e) =>
                      ["auto", "scroll"].includes(
                        getComputedStyle(e).overflowY,
                      ) && e.scrollHeight > e.clientHeight + 1,
                  )
                  .map((e) => e.className),
              };
            }),
            { clipped: false, scrollbars: [] },
          );
          const rect = await popover.boundingBox();
          assert.ok(
            rect.y >= 0 && rect.y + rect.height <= 845,
            "all limits fit in the viewport",
          );
        };
        await assertFits();
        await page
          .getByRole("radio", { name: "Claude", exact: true })
          .locator("..")
          .locator("label")
          .click();
        await page.getByRole("link", { name: "Open free resets" }).waitFor();
        assert.equal(await page.getByRole("tab").count(), 2);
        const after = await popover.boundingBox();
        assert.equal(
          after.height,
          before.height,
          "switching providers preserves natural panel height",
        );
        assert.equal(
          after.y,
          before.y,
          "switching providers preserves popover position",
        );
        await assertFits();
        await page
          .getByRole("radio", { name: "Codex", exact: true })
          .locator("..")
          .locator("label")
          .click();
        await page
          .getByRole("button", { name: "Apply reset", exact: true })
          .waitFor();
        assert.equal((await popover.boundingBox()).height, before.height);
        const claudeDot = page.locator(
          '.account-limits-dot-target[data-account-key="claude-main"]',
        );
        await claudeDot.focus();
        await page.keyboard.press("Enter");
        await page.getByRole("link", { name: "Open free resets" }).waitFor();
        assert.equal(await claudeDot.getAttribute("aria-expanded"), "true");
        assert.equal(
          await page
            .locator('.account-limits-dot-target[aria-current="true"]')
            .getAttribute("data-account-key"),
          "codex-main",
        );
      }

    assert.deepEqual(errors, []);
    console.log(
      "Usage account UI: single account, tabs, lazy load, refresh, low allowance, account-bound reset, and stable five-account popovers at 390/1440 light/dark passed.",
    );
  } finally {
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
