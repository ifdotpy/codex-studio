import {
  test,
  spawnFixture as spawn,
  waitForFixturePort,
  rustUiFixture,
} from "../playwright.ts";
import { required } from "../messages/helpers.ts";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

// The chat composer on a wide touch screen (coarse pointer, 951 px) adds a
// line on Enter; desktop sends on Enter. ⌘/Ctrl + Enter sends on both, and
// Shift never sends.
const layouts = [
  { name: "touch 951x669", viewport: { width: 951, height: 669 }, touch: true },
  {
    name: "desktop 1440x960",
    viewport: { width: 1440, height: 960 },
    touch: false,
  },
];

for (const layout of layouts)
  test(`composer Enter keys at ${layout.name}`, async ({ browser }) => {
    test.setTimeout(60_000);
    const evidence = await mkdtemp(join(tmpdir(), "studio-touch-enter-"));
    const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
    });
    let log = "";
    required(fixture.stderr).on("data", (chunk) => (log += chunk));
    const port = await waitForFixturePort(fixture, {
      label: "Touch Enter",
      diagnostics: () => log,
    });
    const context = await browser.newContext({
      viewport: layout.viewport,
      hasTouch: layout.touch,
      isMobile: layout.touch,
    });
    const page = await context.newPage();
    page.setDefaultTimeout(10_000);
    const sent: string[] = [];
    // Count sends; the request stays open so no reply changes the draft.
    await page.route("**/api/messages", (route) => {
      if (route.request().method() === "POST")
        sent.push(
          (route.request().postDataJSON() as { text: string }).text ?? "",
        );
    });
    await page.goto(`http://127.0.0.1:${port}`);
    await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
    const input = page.locator("#message");
    assert.equal(
      await input.getAttribute("enterkeyhint"),
      layout.touch ? "enter" : null,
    );

    await input.fill("Shift line");
    await input.press("Shift+Enter");
    await input.press("Shift+Control+Enter");
    assert.equal(await input.inputValue(), "Shift line\n\n");
    assert.equal(sent.length, 0);

    await input.fill("Plain Enter");
    await input.press("Enter");
    if (layout.touch) {
      assert.equal(await input.inputValue(), "Plain Enter\n");
      assert.equal(sent.length, 0);
    } else {
      // Desktop Enter sends: the composer clears.
      await page.waitForFunction(
        () =>
          (document.querySelector("#message") as HTMLTextAreaElement).value ===
          "",
      );
    }

    await input.fill("Control Enter");
    await input.press("Control+Enter");
    await page.waitForFunction(
      () =>
        (document.querySelector("#message") as HTMLTextAreaElement).value ===
        "",
    );
    await page.waitForTimeout(500);
    assert.deepEqual(
      sent.map((text) => text.trim()),
      layout.touch ? ["Control Enter"] : ["Plain Enter", "Control Enter"],
    );
    await context.close();
  });
