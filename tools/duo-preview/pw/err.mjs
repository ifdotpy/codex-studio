import { chromium } from "playwright";
const browser = await chromium.launch();
const page = await (await browser.newContext({ viewport: { width: 951, height: 669 }, hasTouch: true })).newPage();
page.on("pageerror", (e) => console.log("PAGEERROR", e.message, (e.stack || "").split("\n").slice(0, 4).join(" | ")));
page.on("console", (m) => { if (m.type() === "error") console.log("CONSOLE", m.text().slice(0, 400)); });
await page.goto(process.argv[2]); await page.waitForTimeout(12000);
console.log("error box:", await page.locator("text=Could not display Studio").count());
await browser.close();
