import { createRequire } from "node:module";
import { mkdirSync, writeFileSync } from "node:fs";
const require = createRequire("/Users/igor/Projects/codex-agents/.worktrees/codex-agents/c6708b22-a9dd-55e2-ab93-e9fcee1a498c/workspaces/client/apps/web/package.json");
const { chromium } = require("playwright");
const out = process.argv[2]; mkdirSync(out, { recursive: true });
const browser = await chromium.launch();
const profiles = [
  { name: "closed", width: 653, height: 951, insets: { top: 82, right: 0, bottom: 34, left: 0 }, hinge: "closed" },
  { name: "portrait", width: 669, height: 951, insets: { top: 82, right: 0, bottom: 34, left: 0 }, hinge: "open" },
];
for (const p of profiles) {
  const context = await browser.newContext({ viewport: { width: p.width, height: p.height }, hasTouch: true, isMobile: true, deviceScaleFactor: 2, colorScheme: "dark", serviceWorkers: "block" });
  const page = await context.newPage();
  await page.addInitScript((p) => {
    const original = window.matchMedia.bind(window);
    window.matchMedia = (q) => { const r = original(q); if (/pointer: coarse/.test(q)) Object.defineProperty(r, "matches", { value: true }); return r; };
    Object.defineProperty(navigator, "maxTouchPoints", { value: 5 });
    window.__studioNativeViewport = { version: 1, state: { insets: p.insets, keyboard: { overlap: 0, duration: 0, curve: 7 } } };
    const apply = () => { const root = document.documentElement; if (!root) return;
      for (const [k, v] of Object.entries(p.insets)) root.style.setProperty("--native-safe-" + k, v + "px");
      root.dataset.nativeHinge = p.hinge;
      root.style.setProperty("--native-status-column-bottom", "82px");
      root.style.setProperty("--native-status-column-left", p.width - 124 + "px"); };
    apply(); document.addEventListener("DOMContentLoaded", apply, { once: true });
  }, p);
  await page.goto("http://127.0.0.1:8794/");
  await page.locator("#composer").waitFor({ timeout: 60000 });
  const rect = (sel) => page.evaluate((sel) => [...document.querySelectorAll(sel)].filter((n) => n.getClientRects().length).map((n) => { const r = n.getBoundingClientRect(); return [r.x, r.y, r.width, r.height].map(Math.round); }), sel);
  // Drawer open: open it with the rail sidebar button when it is closed.
  if (!(await page.locator("#sidebar").isVisible())) await page.locator(".edge-rail button").first().click();
  await page.locator("#sidebar").waitFor();
  await page.waitForTimeout(800);
  const drawer = { inner: await rect(".mantine-Drawer-inner"), content: await rect(".mantine-Drawer-content"), sidebar: await rect("#sidebar"), rail: await rect(".edge-rail") };
  await page.screenshot({ path: `${out}/${p.name}-drawer.png` });
  await page.getByPlaceholder("Filter sidebar").fill("attar");
  await page.getByText("Найти ветку переноса на attar").first().click();
  await page.waitForTimeout(6000);
  const chat = { sidebarVisible: await page.locator("#sidebar").isVisible(), rail: await rect(".edge-rail"), railButtons: await rect(".edge-rail .edge-rail-button"), title: await rect(".duo-transcript-bar"), composer: await rect("#composer"), attrs: await page.evaluate(() => [...document.documentElement.attributes].map((a) => a.name).filter((n) => /strip|duo|touch|native/.test(n))) };
  await page.screenshot({ path: `${out}/${p.name}-chat.png` });
  writeFileSync(`${out}/${p.name}.json`, JSON.stringify({ drawer, chat }, null, 1));
  console.log(p.name, JSON.stringify({ drawer, chat }));
  await context.close();
}
await browser.close();
