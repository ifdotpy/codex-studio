import { chromium } from "playwright";
const [url, out] = process.argv.slice(2);
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 669, height: 951 }, hasTouch: true, deviceScaleFactor: 2, colorScheme: "dark" });
const page = await context.newPage();
const cdp = await context.newCDPSession(page);
await cdp.send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 82, right: 0, bottom: 34, left: 0 } });
await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
await page.goto(url);
await page.waitForTimeout(8000);
const info = await page.evaluate(() => {
  const r = (n) => { const b = n.getBoundingClientRect(); return [Math.round(b.x), Math.round(b.y), Math.round(b.width), Math.round(b.height)]; };
  const hits = [...document.querySelectorAll("body *")].filter((n) => n.childElementCount === 0 && /Servers and chats/.test(n.textContent || ""));
  return {
    title: hits.map((n) => { const chain = []; let x = n; for (let i = 0; i < 6 && x; i++, x = x.parentElement) chain.push(`${x.tagName.toLowerCase()}.${String(x.className).slice(0, 50)} ${JSON.stringify(r(x))} pos=${getComputedStyle(x).position} vis=${getComputedStyle(x).visibility}`); return chain; }),
    header: [...document.querySelectorAll("header")].map((h) => `${String(h.className).slice(0, 60)} ${JSON.stringify(r(h))}`),
    root: r(document.getElementById("root")), rootPad: getComputedStyle(document.getElementById("root")).padding,
  };
});
console.log(JSON.stringify(info, null, 1));
await page.screenshot({ path: out });
await browser.close();
