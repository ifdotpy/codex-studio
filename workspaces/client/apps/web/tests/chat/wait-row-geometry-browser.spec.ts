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

// A lead waiting for its workers shows "Waiting for N agents" with animated
// dots. A touch summary is 44 px tall; its text must stay on the dots' line.
const layouts = [
  { name: "touch 951x669", viewport: { width: 951, height: 669 }, touch: true },
  {
    name: "desktop 1440x960",
    viewport: { width: 1440, height: 960 },
    touch: false,
  },
];

for (const layout of layouts)
  test(`wait dots stay on the label line at ${layout.name}`, async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    const evidence = await mkdtemp(join(tmpdir(), "studio-wait-row-"));
    const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        FIXTURE_WORKER_COUNT: "2",
        FIXTURE_HOLD: "workers",
      },
    });
    let log = "";
    required(fixture.stderr).on("data", (chunk) => (log += chunk));
    const port = await waitForFixturePort(fixture, {
      label: "Wait row geometry",
      diagnostics: () => log,
    });
    const context = await browser.newContext({
      viewport: layout.viewport,
      hasTouch: layout.touch,
      isMobile: layout.touch,
    });
    const page = await context.newPage();
    page.setDefaultTimeout(10_000);
    await page.goto(`http://127.0.0.1:${port}`);
    await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
    const row = page.locator('.agent-phase[data-wait-state="live"]');
    await row.getByText("Waiting for 2 agents").first().waitFor();
    await page.screenshot({
      path: test.info().outputPath(`wait-row-${layout.viewport.width}.png`),
    });
    const geometry = await row.evaluate((element) => {
      const dots = element.querySelector(".wait-dots")!.getBoundingClientRect();
      const range = document.createRange();
      range.selectNodeContents(element.querySelector("summary")!);
      const lines = [...range.getClientRects()];
      const text = range.getBoundingClientRect();
      return {
        lines: new Set(lines.map((line) => Math.round(line.top))).size,
        dotsRight: dots.right,
        textLeft: text.left,
        offset: Math.abs(
          (dots.top + dots.bottom) / 2 - (text.top + text.bottom) / 2,
        ),
      };
    });
    assert.equal(geometry.lines, 1);
    assert.ok(
      geometry.dotsRight <= geometry.textLeft,
      JSON.stringify(geometry),
    );
    assert.ok(geometry.offset <= 2, JSON.stringify(geometry));
    await context.close();
  });
