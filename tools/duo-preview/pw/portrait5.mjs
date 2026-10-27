import { chromium } from "playwright";
const [url, out] = process.argv.slice(2);
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 669, height: 951 }, hasTouch: true, deviceScaleFactor: 2, colorScheme: "dark" });
await context.addInitScript(() => {
  const state = { insets: { top: 82, right: 0, bottom: 34, left: 0 }, keyboard: null, statusColumn: { x: 540, y: 0, width: 129, height: 82 } };
  Object.defineProperty(window, "__studioNativeViewport", { value: { version: 1, get state() { return state; }, receive() {} } });
  const set = () => {
    const r = document.documentElement.style;
    for (const [k, v] of Object.entries(state.insets)) r.setProperty(`--native-safe-${k}`, `${v}px`);
    r.setProperty("--native-status-column-bottom", "82px");
    r.setProperty("--native-status-column-left", process.env.STATUS_LEFT || "540px");
  };
  set();
});
const page = await context.newPage();
const measure = (tag) => page.evaluate((tag) => {
  const r = (n) => { if (!n) return null; const b = n.getBoundingClientRect(); return [b.x, b.y, b.width, b.height].map(Math.round); };
  return { tag, strip: document.documentElement.hasAttribute("data-viewport-top-strip"),
    rootPad: getComputedStyle(document.getElementById("root")).paddingTop,
    bar: getComputedStyle(document.querySelector(".server-mobile-toggle") || document.body).display + " " + JSON.stringify(r(document.querySelector(".server-mobile-toggle"))),
    rail: r(document.querySelector(".edge-rail")),
    buttons: [...document.querySelectorAll(".edge-rail .edge-rail-button")].map((b) => b.getAttribute("aria-label") + " " + JSON.stringify(r(b))),
    header: [...document.querySelectorAll("header button")].slice(0, 6).map((b) => (b.getAttribute("aria-label") || b.textContent || "").trim().slice(0, 24) + " " + JSON.stringify(r(b))) };
}, tag);
await page.goto(url);
await page.waitForTimeout(15000);
console.log(JSON.stringify(await measure("start")));
await page.screenshot({ path: out.replace(".png", "-start.png") });
await page.locator(".server-mobile-toggle").click();
await page.waitForTimeout(1500);
await page.getByPlaceholder("Filter sidebar").fill("attar");
await page.waitForTimeout(1500);
await page.getByText("Найти ветку переноса на attar").first().click();
await page.waitForTimeout(10000);
console.log(JSON.stringify(await measure("chat")));
await page.screenshot({ path: out });
await browser.close();
