import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const repo = fileURLToPath(new URL("../", import.meta.url));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-original-typography-"));
const fixture = spawn(
  process.env.PYTHON_BIN || "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "1" },
  },
);
let browser;
let log = "";
fixture.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(log)));
  });
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(8000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const storageKey = "codex-studio-preferences-v1";
  // A saved preference from before the preset existed must retain its values.
  await page.addInitScript((key) => {
    if (!localStorage.getItem(key))
      localStorage.setItem(
        key,
        JSON.stringify({
          theme: "dark",
          sidebarFontSize: 22,
          mainFontSize: 24,
          fontFamily: "georgia",
          contentWidth: 80,
          sidebarShortcut: "Meta+b",
          showMessageAvatars: true,
        }),
      );
  }, storageKey);
  await page.goto(`http://127.0.0.1:${port}`);
  await page.locator("#message").waitFor();
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  const size = (selector) =>
    page
      .locator(selector)
      .first()
      .evaluate((el) => getComputedStyle(el).fontSize);
  assert.equal(await size("#messages .prose"), "24px");
  const openSettings = async () => {
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const dialog = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await dialog.getByRole("tab", { name: "Appearance", exact: true }).click();
    return dialog;
  };
  let settings = await openSettings();
  assert.equal(
    await settings
      .getByLabel("Studio text style", { exact: true })
      .inputValue(),
    "custom",
  );
  await settings
    .getByLabel("Studio text style", { exact: true })
    .selectOption("original");
  const checkOriginal = async () => {
    // These are the sizes in 4a0746c^, before global typography overrides.
    for (const [selector, expected] of [
      [".chat-row .row-copy strong", "13px"],
      ["#conversation-title", "15px"],
      ["#conversation-status", "11px"],
      ["#messages .prose", "14px"],
      ["#message", "14px"],
    ])
      assert.equal(await size(selector), expected, selector);
    for (const selector of [
      ".chat-row .row-copy strong",
      "#messages .prose",
      "#message",
    ])
      assert.equal(
        await page
          .locator(selector)
          .first()
          .evaluate((el) =>
            getComputedStyle(el).fontFamily.replace(
              /"system-ui"/g,
              "BlinkMacSystemFont",
            ),
          ),
        'Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
        selector,
      );
    assert.equal(
      await page.evaluate(() => {
        const code = document.createElement("code");
        document.querySelector("#messages .prose").append(code);
        const family = getComputedStyle(code).fontFamily;
        code.remove();
        return family;
      }),
      "SFMono-Regular, Consolas, monospace",
    );
  };
  await checkOriginal();
  assert.equal(
    await settings
      .getByLabel("Studio font family", { exact: true })
      .isDisabled(),
    true,
  );
  const stored = await page.evaluate(
    (key) => JSON.parse(localStorage.getItem(key)),
    storageKey,
  );
  assert.equal(stored.typography, "original");
  assert.equal(stored.contentWidth, 80);
  assert.equal(stored.theme, "dark");
  assert.equal(stored.showMessageAvatars, true);
  assert.equal(stored.mainFontSize, 24);
  await page.keyboard.press("Escape");
  await page.reload();
  await page.locator("#message").waitFor();
  await checkOriginal();
  await page.screenshot({ path: join(evidence, "original.png") });
  settings = await openSettings();
  assert.equal(
    await settings
      .getByLabel("Studio text style", { exact: true })
      .inputValue(),
    "original",
  );
  await settings
    .getByLabel("Studio text style", { exact: true })
    .selectOption("custom");
  assert.equal(await size(".chat-row .row-copy strong"), "22px");
  assert.equal(await size("#messages .prose"), "24px");
  assert.match(
    await page
      .locator("#message")
      .evaluate((el) => getComputedStyle(el).fontFamily),
    /Georgia/,
  );
  assert.equal(
    await settings
      .getByLabel("Studio font family", { exact: true })
      .isDisabled(),
    false,
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS: original fonts and size hierarchy, reload, legacy preferences, and preserved custom values. ${evidence}`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
