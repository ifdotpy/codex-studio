// Opt-in check of a deployed gateway (docs/vault.md, "Gateway") in headless Chromium with a
// virtual PRF authenticator, from a device in the tailnet. Use a test vault: the paper key (24
// words or the AGE form) is read from a file. The added passkey is removed again.
//
//   VAULT_SITE=https://desk.example.com/<project>/ VAULT_PAPER_FILE=paper.txt \
//   VAULT_SOURCE=<project folder> VAULT_FILE=<path in the project> node tests/live.e2e.mjs
import { chromium } from "playwright";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const site = process.env.VAULT_SITE;
const paper = readFileSync(process.env.VAULT_PAPER_FILE, "utf8").trim();
const file = process.env.VAULT_FILE;
const want = createHash("sha256")
  .update(readFileSync(join(process.env.VAULT_SOURCE, file)))
  .digest("hex");
const sha = (data) => createHash("sha256").update(data).digest("hex");

const browser = await chromium.launch();
try {
  const context = await browser.newContext({ acceptDownloads: true });
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      hasPrf: true,
      automaticPresenceSimulation: true,
    },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on(
    "console",
    (message) => message.type() === "error" && errors.push(message.text()),
  );

  const unlocked = () =>
    page.waitForSelector("#columns:not([hidden]), #list:not([hidden])", {
      timeout: 60000,
    });
  const downloadFile = async () => {
    await page.fill("#filter", file.split("/").pop());
    await page.click(`#list-rows .item:has-text('${file}')`);
    await page.keyboard.press(" ");
    const download = page.waitForEvent("download", { timeout: 60000 });
    await page.click("#ql-download");
    assert.equal(sha(readFileSync(await (await download).path())), want);
    await page.keyboard.press("Escape");
  };

  await page.goto(site);
  await page.waitForSelector("#unlock:not([hidden])", { timeout: 30000 });
  if (!(await page.isVisible("#paper"))) await page.click("#paper-box summary");
  await page.fill("#paper", paper);
  await page.click("#paper-form button");
  await unlocked();
  await downloadFile();

  const names = () =>
    page.$$eval("#passkey-list li span", (spans) =>
      spans.map((s) => s.textContent),
    );
  await page.click("#passkeys");
  await page.waitForSelector("#passkey-list li");
  const before = await names();
  await page.fill("#label", "live check");
  await page.click("#add-form button");
  await page.waitForFunction(
    (n) => document.querySelectorAll("#passkey-list li span").length === n + 1,
    before.length,
    { timeout: 30000 },
  );
  const added = (await names()).find((name) => !before.includes(name));
  await page.click("#close-passkeys");
  await page.reload();
  await page.waitForSelector("#with-passkey:not([hidden])", { timeout: 30000 });
  await page.click("#with-passkey");
  await unlocked();
  await downloadFile();

  // Remove the passkey that this check added.
  await page.click("#passkeys");
  await page.click(`#passkey-list li:has(span:text-is("${added}")) button`);
  await page.waitForFunction(
    (n) => document.querySelectorAll("#passkey-list li span").length === n,
    before.length,
    { timeout: 30000 },
  );
  assert.deepEqual(errors, []);
  console.log("live gateway check: passed");
} finally {
  await browser.close();
}
