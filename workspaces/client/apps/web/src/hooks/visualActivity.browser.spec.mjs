import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../../tests/playwright.mjs";

test("hidden visual rows stop timers and rate renders and resume current values", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const { createServer } =
    await import("../../node_modules/vite/dist/node/index.js");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-visual-activity-"));
  const entry = `
    import React from 'react';
    import {createRoot} from 'react-dom/client';
    import TokenRate from '/src/components/TokenRate.tsx';
    import ReasoningDuration from '/src/components/conversation/transcript/ReasoningDuration.tsx';
    import ChatStatus from '/src/components/agents/ChatStatus.tsx';
    import {receiveTokenRate,workerRateKey,configureTokenRateStream} from '/src/usage/tokenRate.ts';
    import '/src/visual-activity.css';
    import '/src/components/team-navigation.css';
    const e=React.createElement;
    window.commits={};
    window.rateInterests=new Set();
    configureTokenRateStream(listener=>{
      window.rateInterests.add(listener);
      return ()=>window.rateInterests.delete(listener);
    });
    window.rate=(id,value)=>receiveTokenRate(id,{data:JSON.stringify({turnId:'turn',active:true,rate:value,outputTokens:100})});
    window.workerRate=value=>window.rate(workerRateKey('team','worker'),value);
    for(const id of ['near','far','closed'])window.rate(id,20);
    const card=(id,reason=true)=>e(React.Profiler,{id,onRender:()=>window.commits[id]=(window.commits[id]||0)+1},
      e('div',{id,className:'card'},
        e(ChatStatus,{status:{kind:'working',label:'Working'}}),
        e(TokenRate,{agent:{id,turnId:'turn'}}),
        reason&&e(ReasoningDuration,{item:{id:'reason:'+id,reasoningMs:3000,reasoningSince:100,reasoningObservedAt:100}})));
    const root=createRoot(document.querySelector('#root'));
    root.render(e(React.StrictMode,null,card('near'),card('empty',false),
      e('div',{id:'team'},e('span',{className:'worker-meta'},'Worker ',e(TokenRate,{agent:{id:'worker',rootId:'team'},variant:'worker'}))),
      e('details',{id:'fold'},e('summary',null,'Show work'),card('closed')),card('far')));
    window.unmount=()=>root.unmount();
  `;
  const server = await createServer({
    configFile: false,
    cacheDir,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "visual-activity-check",
        resolveId(id) {
          if (id === "/visual-activity-entry") return "\0visual-activity-entry";
        },
        load(id) {
          if (id === "\0visual-activity-entry") return entry;
        },
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (request.url !== "/visual-activity-check") return next();
            response.setHeader("Content-Type", "text/html");
            response.end(`<!doctype html><title>Visual activity check</title>
              <style>body{margin:0}.card{display:flex;gap:12px;height:40px;width:600px}#far{margin-top:2000px}</style>
              <div id="root"></div>
              <script type="module" src="/visual-activity-entry"></script>`);
          });
        },
      },
    ],
  });
  const page = await browser.newPage({ viewport: { width: 800, height: 600 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    const timers = new Map();
    const interval = window.setInterval;
    const clear = window.clearInterval;
    window.visualMetrics = { ticks: 0, observers: 0 };
    window.activeVisualTimers = () => timers.size;
    window.setInterval = (callback, delay, ...args) => {
      if (delay !== 1000) return interval(callback, delay, ...args);
      const id = interval(() => {
        window.visualMetrics.ticks++;
        callback(...args);
      }, delay);
      timers.set(id, true);
      return id;
    };
    window.clearInterval = (id) => {
      timers.delete(id);
      clear(id);
    };
    const Observer = window.IntersectionObserver;
    window.IntersectionObserver = class extends Observer {
      constructor(...args) {
        super(...args);
        window.visualMetrics.observers++;
        this.active = true;
      }
      disconnect() {
        if (this.active) window.visualMetrics.observers--;
        this.active = false;
        super.disconnect();
      }
    };
    window.testingHidden = false;
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => window.testingHidden,
    });
    window.setTestingHidden = (hidden) => {
      window.testingHidden = hidden;
      document.dispatchEvent(new Event("visibilitychange"));
    };
  });
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/visual-activity-check`,
    );
    await expect(page.locator("#near .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await expect(page.locator("#far .token-rate")).toHaveAttribute(
      "data-visual-active",
      "false",
    );
    await expect(page.locator("#closed .token-rate")).toHaveAttribute(
      "data-visual-active",
      "false",
    );
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(1);
    expect(await page.evaluate(() => window.visualMetrics.observers)).toBe(1);
    expect(await page.evaluate(() => window.rateInterests.size)).toBe(2);
    expect(
      await page
        .locator("#far .chat-status svg")
        .evaluate((node) => getComputedStyle(node).animationPlayState),
    ).toBe("paused");
    expect(
      await page
        .locator("#near .chat-status svg")
        .evaluate((node) => getComputedStyle(node).animationPlayState),
    ).toBe("running");
    await page.evaluate(() => {
      window.commits = {};
      for (let i = 0; i < 100; i++) {
        window.rate("far", 30 + i);
        window.rate("closed", 30 + i);
      }
      window.rate("empty", 42);
    });
    await expect(page.locator("#empty .token-rate")).toHaveText("42 tok/s");
    expect(await page.evaluate(() => window.commits.far || 0)).toBe(0);
    expect(await page.evaluate(() => window.commits.closed || 0)).toBe(0);
    expect(await page.locator("#team .token-rate").boundingBox()).toBeNull();
    await page.evaluate(() => window.workerRate(43));
    await expect(page.locator("#team .token-rate")).toHaveText("43 tok/s");
    await expect(page.locator("#team .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await page.locator("#fold summary").click();
    await expect(page.locator("#closed .token-rate")).toHaveText("129 tok/s");
    expect(await page.evaluate(() => window.rateInterests.size)).toBe(3);
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(2);
    await page.evaluate(() => window.setTestingHidden(true));
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(0);
    expect(await page.evaluate(() => window.rateInterests.size)).toBe(0);
    const ticks = await page.evaluate(() => window.visualMetrics.ticks);
    await page.waitForTimeout(1200);
    expect(await page.evaluate(() => window.visualMetrics.ticks)).toBe(ticks);
    await page.evaluate(() => {
      window.rate("near", 88);
      window.setTestingHidden(false);
    });
    await expect(page.locator("#near .token-rate")).toHaveAttribute(
      "data-rate",
      "88",
    );
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(2);
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    await expect(page.locator("#far .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await expect(page.locator("#far .token-rate")).toHaveText("129 tok/s");
    expect(await page.evaluate(() => window.rateInterests.size)).toBe(1);
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(1);
    await expect
      .poll(async () => {
        const text = await page.locator("#far .reasoning-time").textContent();
        return Number.parseInt(text) >= 4;
      })
      .toBe(true);
    await page.evaluate(() => window.unmount());
    expect(await page.evaluate(() => window.activeVisualTimers())).toBe(0);
    expect(await page.evaluate(() => window.rateInterests.size)).toBe(0);
    expect(await page.evaluate(() => window.visualMetrics.observers)).toBe(0);
    // Without an observer, keep visible-tab updates active after hide/resume.
    // There is no scroll observer to reactivate an offscreen row in this mode.
    await page.addInitScript(() => {
      window.IntersectionObserver = undefined;
    });
    await page.reload();
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(2);
    await page.evaluate(() => window.setTestingHidden(true));
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(0);
    await page.evaluate(() => {
      window.rate("far", 91);
      window.setTestingHidden(false);
    });
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(2);
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    await expect(page.locator("#far .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await expect(page.locator("#far .token-rate")).toHaveText("91 tok/s");
    await page.locator("#fold summary").click();
    await expect
      .poll(() => page.evaluate(() => window.activeVisualTimers()))
      .toBe(3);
    await page.evaluate(() => window.unmount());
    expect(await page.evaluate(() => window.activeVisualTimers())).toBe(0);
    expect(errors).toEqual([]);
  } finally {
    await page.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
