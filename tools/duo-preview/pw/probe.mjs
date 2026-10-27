// Usage: node probe.mjs <url> <out.png> [action]   (action: menu)
import { chromium } from "playwright";
const [url, out, action] = process.argv.slice(2);
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 951, height: 669 }, hasTouch: true, isMobile: false, deviceScaleFactor: 2, colorScheme: "dark" });
const page = await context.newPage();
const cdp = await context.newCDPSession(page);
await cdp.send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 0, right: 82, bottom: 34, left: 0 } });
await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
await page.goto(url);
await page.waitForSelector("#composer", { timeout: 60000 }).catch(() => {});
await page.waitForTimeout(4000);
const rect = (sel) => page.evaluate((s) => [...document.querySelectorAll(s)].slice(0, 6).map((n) => { const r = n.getBoundingClientRect(); return [n.getAttribute("aria-label") || n.className?.toString().slice(0, 40), Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; }), sel);
if (action === "menu") {
  await page.locator('.edge-rail [aria-label="Chat actions"]').click();
  await page.waitForTimeout(800);
  console.log("menu", JSON.stringify(await rect(".mantine-Menu-dropdown")));
}
console.log("rail", JSON.stringify(await rect(".edge-rail, .edge-rail .edge-rail-button")));
console.log("header", JSON.stringify(await rect(".sidebar-header, .sidebar-header > *, .sidebar-header .mantine-Indicator-indicator, .sidebar-header [class*=badge], .sidebar-header [class*=Badge]")));
console.log("projects", JSON.stringify(await rect(".projects-heading, .projects-heading > *")));
console.log("chatScroll", JSON.stringify(await rect(".chat-scroll")));
console.log("terminal", JSON.stringify(await rect(".terminal-dock, .sidebar .terminal-dock, [class*=terminal-shell]")));
await page.screenshot({ path: out });
await browser.close();
