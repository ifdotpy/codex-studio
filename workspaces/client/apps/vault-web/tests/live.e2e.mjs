// Opt-in check of a deployed vault (docs/vault.md) in headless Chromium with a virtual PRF
// authenticator. Use a test vault: the paper key is read from a file.
//
//   VAULT_SITE=https://… VAULT_PAPER_FILE=paper.txt VAULT_SOURCE=<project folder> node tests/live.e2e.mjs
import { chromium } from "playwright";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const site = process.env.VAULT_SITE;
const paper = readFileSync(process.env.VAULT_PAPER_FILE, "utf8").match(
  /AGE-SECRET-KEY-1[0-9A-Z]+/,
)[0];
const source = process.env.VAULT_SOURCE;
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
  await page.goto(site);
  await page.waitForSelector("#unlock:not([hidden])", { timeout: 30000 });

  await page.fill("#paper", paper);
  await page.click("#paper-form button");
  await page.waitForSelector("#browse:not([hidden])", { timeout: 30000 });
  await page.click("text=src/");
  const download = page.waitForEvent("download");
  await page.click("text=app.py");
  assert.equal(
    sha(readFileSync(await (await download).path())),
    sha(readFileSync(join(source, "src/app.py"))),
  );

  await page.click("#passkeys");
  await page.fill("#label", "live check");
  await page.click("#add-form button");
  await page.waitForSelector("#passkey-list li button", { timeout: 30000 });
  await page.reload();
  await page.waitForSelector("#with-passkey:not([hidden])", { timeout: 30000 });
  await page.click("#with-passkey");
  await page.waitForSelector("#browse:not([hidden])", { timeout: 30000 });
  await page.click("text=assets/");
  const blob = page.waitForEvent("download");
  await page.click("text=blob.bin");
  assert.equal(
    sha(readFileSync(await (await blob).path())),
    sha(readFileSync(join(source, "assets/blob.bin"))),
  );

  await page.click("#passkeys");
  await page.click("#passkey-list li button");
  await page.waitForFunction(
    () => !document.querySelector("#passkey-list li button"),
    null,
    { timeout: 30000 },
  );
  assert.equal(await page.textContent("#status"), "");
  assert.deepEqual(errors, []);
  console.log("live vault check: passed");
} finally {
  await browser.close();
}
