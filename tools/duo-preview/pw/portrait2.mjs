import { chromium } from "playwright";
const [url, out] = process.argv.slice(2);
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 669, height: 951 }, hasTouch: true, deviceScaleFactor: 2, colorScheme: "dark" });
await context.addInitScript(() => {
  // Emulate the iOS shell's viewport API (portrait: top strip 82, Home 34).
  const state = { insets: { top: 82, right: 0, bottom: 34, left: 0 }, keyboard: null };
  Object.defineProperty(window, "__studioNativeViewport", { value: { version: 1, get state() { return state; }, receive() {} } });
  for (const [k, v] of Object.entries(state.insets)) document.documentElement.style.setProperty(`--native-safe-${k}`, `${v}px`);
});
const page = await context.newPage();
const cdp = await context.newCDPSession(page);
await cdp.send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 82, right: 0, bottom: 34, left: 0 } });
await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
await page.goto(url);
await page.waitForTimeout(20000);
const info = await page.evaluate(() => {
  const r = (n) => { if (!n) return null; const b = n.getBoundingClientRect(); return [Math.round(b.x), Math.round(b.y), Math.round(b.width), Math.round(b.height)]; };
  return {
    attrs: [...document.documentElement.attributes].map((a) => a.name).filter((n) => /strip|rail|viewport/.test(n)),
    rootPad: getComputedStyle(document.getElementById("root")).padding,
    rail: r(document.querySelector(".edge-rail")), railSide: document.querySelector(".edge-rail")?.dataset.side,
    buttons: [...document.querySelectorAll(".edge-rail .edge-rail-button")].map((b) => [b.getAttribute("aria-label"), ...r(b)]),
    serversBar: r(document.querySelector(".server-mobile-toggle")), serversBarDisplay: document.querySelector(".server-mobile-toggle") && getComputedStyle(document.querySelector(".server-mobile-toggle")).display,
    header: [...document.querySelectorAll("header")].map((h) => `${String(h.className).slice(0, 40)} ${JSON.stringify(r(h))}`),
    messages: r(document.querySelector("#messages")),
  };
});
console.log(JSON.stringify(info, null, 1));
await page.screenshot({ path: out });
await browser.close();
