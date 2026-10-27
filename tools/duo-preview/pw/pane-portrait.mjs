import { createRequire } from "node:module";
import { mkdirSync, writeFileSync } from "node:fs";
const require = createRequire("/Users/igor/Projects/codex-agents/.worktrees/codex-agents/c6708b22-a9dd-55e2-ab93-e9fcee1a498c/workspaces/client/apps/web/package.json");
const { chromium } = require("playwright");
const [out, url = "http://127.0.0.1:8794/"] = process.argv.slice(2); mkdirSync(out, { recursive: true });
const browser = await chromium.launch();
const profiles = [].concat(
  { name: "open-team", width: 951, height: 669, insets: { top: 0, right: 82, bottom: 34, left: 0 }, hinge: "open", column: [867, 124], band: [455.5, 0, 40, 669], action: "team" },
  { name: "portrait-sidebar", width: 669, height: 951, insets: { top: 82, right: 0, bottom: 34, left: 0 }, hinge: "open", column: [525, 82], band: [0, 455.5, 669, 40], action: "sidebar" },
  { name: "closed-team", width: 653, height: 951, insets: { top: 82, right: 0, bottom: 34, left: 0 }, hinge: "closed", column: [529, 82], band: null, action: "team" },
]).filter((p) => p.name === "portrait-sidebar");
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
      root.style.setProperty("--native-status-column-left", p.column[0] + "px");
      root.style.setProperty("--native-status-column-bottom", p.column[1] + "px");
      if (p.band) ["left", "top", "width", "height"].forEach((k, i) => root.style.setProperty("--native-hinge-" + k, p.band[i] + "px")); };
    apply(); document.addEventListener("DOMContentLoaded", apply, { once: true });
  }, p);
  await page.goto(url);
  await page.locator("#composer").waitFor({ timeout: 60000 });
  const rail = page.locator(".edge-rail");
  // Open the iPhone Duo chat (it has a team): read-only navigation.
  if (!(await page.locator("#sidebar").isVisible())) await rail.locator("button").first().click();
  await page.getByPlaceholder("Filter sidebar").fill("iPhone Duo");
  await page.getByText("iPhone Duo", { exact: true }).first().click();
  await page.waitForTimeout(5000);
  if (p.action === "team") await rail.locator('button[aria-label^="Team"]').click();
  else if (!(await page.locator("#sidebar").isVisible())) await rail.locator("button").first().click();
  await page.waitForTimeout(2500);
  const rect = (sel) => page.evaluate((sel) => [...document.querySelectorAll(sel)].filter((n) => n.getClientRects().length).map((n) => { const r = n.getBoundingClientRect(); return [r.x, r.y, r.width, r.height].map(Math.round); }), sel);
  const data = { attrs: await page.evaluate(() => [...document.documentElement.attributes].map((a) => a.name).filter((n) => /pane|duo|strip|touch/.test(n))),
    team: await rect("#team"), sidebar: await rect("#sidebar"), conversation: await rect("#conversation"), composer: await rect("#composer"), rail: await rect(".edge-rail"), drawer: await rect(".mantine-Drawer-inner"), title: await rect(".duo-transcript-bar") };
  await page.screenshot({ path: `${out}/${p.name}.png` });
  writeFileSync(`${out}/${p.name}.json`, JSON.stringify(data, null, 1));
  console.log(p.name, JSON.stringify(data));
  await context.close();
}
await browser.close();
