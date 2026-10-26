import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
  readTestState,
} from "../../../tests/playwright.ts";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("fresh phone entry shows the board; explicit opens, Back and resume preserve intent", async ({
  browser,
}) => {
  test.setTimeout(90000);
  const root = await mkdtemp(join(tmpdir(), "studio-phone-fleet-"));
  const evidence = test.info().outputPath("screenshots");
  await mkdir(evidence, { recursive: true });
  const fixture = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      FIXTURE_SEED: "question",
      FIXTURE_HOLD: "lead,other",
      FIXTURE_WORKER_COUNT: "0",
    },
  });
  let errors = "";
  fixture.stderr.on("data", (chunk) => {
    errors += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(errors)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await readTestState(origin);
    const lead = snapshot.runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    const other = snapshot.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    for (const scheme of ["light", "dark"]) {
      for (const width of [390, 320]) {
        const context = await browser.newContext({
          viewport: { width, height: 844 },
          isMobile: true,
          hasTouch: true,
          colorScheme: scheme,
        });
        const page = await context.newPage();
        await page.addInitScript(
          ({ scheme, lead }) => {
            localStorage.setItem("mantine-color-scheme-value", scheme);
            localStorage.setItem("codex-mobile-opened", JSON.stringify(lead));
          },
          { scheme, lead: lead.id },
        );
        await page.goto(origin);
        const board = page.locator("#sidebar");
        await expect(board).toBeInViewport({ ratio: 1 });
        await expect(
          board.getByRole("region", { name: "Needs you" }),
        ).toBeVisible();
        await expect(board.locator(".sidebar-project").first()).toBeVisible();
        await expect(
          board.locator('.chat-row[aria-current="true"]'),
        ).toHaveCount(0);
        await expect(page.locator("#conversation-title")).not.toContainText(
          lead.name,
        );
        await page.screenshot({
          path: join(evidence, `phone-home-${scheme}-${width}.png`),
        });
        const strip = board
          .locator(".sidebar-project .chat-row")
          .filter({ hasText: lead.name })
          .first();
        await strip.click();
        await expect(board).toBeHidden();
        await expect(page.locator("#conversation-title")).toContainText(
          lead.name,
        );
        const back = page.getByRole("button", {
          name: "Back to chats",
          exact: true,
        });
        await expect(back).toHaveText("Chats");
        await page.locator("#message").fill("Saved phone draft");
        await page.screenshot({
          path: join(evidence, `phone-detail-${scheme}-${width}.png`),
        });
        await back.click();
        await expect(board).toBeInViewport({ ratio: 1 });
        await expect(
          page.getByRole("dialog", { name: "Conversations", exact: true }),
        ).toHaveAttribute("aria-modal", "true");
        await expect(page.locator(".workspace-frame")).toHaveAttribute(
          "inert",
          "",
        );
        await expect(
          page.getByRole("button", { name: "Chat actions", exact: true }),
        ).toHaveCount(0);
        await expect(
          board.locator(".chat-row").filter({ hasText: lead.name }).first(),
        ).toBeFocused();
        const activeMenu = strip.locator("..").locator(".row-actions");
        await expect(activeMenu).toHaveCSS("opacity", "1");
        const target = await activeMenu.boundingBox();
        expect(target.width).toBeGreaterThanOrEqual(44);
        expect(target.height).toBeGreaterThanOrEqual(44);
        await strip.click();
        await expect(page.locator(".workspace-frame")).not.toHaveAttribute(
          "inert",
          "",
        );
        await expect(page.locator("#message")).toHaveValue("Saved phone draft");
        // Returning from another app or bfcache preserves the current detail.
        await page.evaluate(() =>
          (() => {
            Object.defineProperty(document, "visibilityState", {
              configurable: true,
              get: () => "hidden",
            });
            document.dispatchEvent(new Event("visibilitychange"));
            Object.defineProperty(document, "visibilityState", {
              configurable: true,
              get: () => "visible",
            });
            document.dispatchEvent(new Event("visibilitychange"));
            window.dispatchEvent(
              new PageTransitionEvent("pageshow", { persisted: true }),
            );
            delete document.visibilityState;
          })(),
        );
        await expect(board).toBeHidden();
        await expect(page.locator("#conversation-title")).toContainText(
          lead.name,
        );
        await expect(page.locator("#message")).toHaveValue("Saved phone draft");
        await back.click();
        await board
          .locator(".sidebar-project .chat-row")
          .filter({ hasText: other.name })
          .first()
          .click();
        await expect(board).toBeHidden();
        await expect(page.locator("#conversation-title")).toContainText(
          other.name,
        );
        await back.click();
        await expect(
          board.locator(".chat-row").filter({ hasText: other.name }).first(),
        ).toBeFocused();
        await page.reload();
        await expect(board).toBeInViewport({ ratio: 1 });
        await expect(
          board.locator('.chat-row[aria-current="true"]'),
        ).toHaveCount(0);
        await context.close();
      }
    }
    const classic = await browser.newPage({
      viewport: { width: 390, height: 844 },
    });
    await classic.goto(
      `${origin}/?studio-server=local&studio-navigation=classic`,
    );
    await expect(classic.locator("#sidebar")).toBeInViewport({ ratio: 1 });
    await classic.evaluate(
      (agentId) =>
        window.dispatchEvent(
          new CustomEvent("studio-navigate", {
            detail: { agentId, section: "messages" },
          }),
        ),
      other.id,
    );
    await expect(classic.locator("#sidebar")).toBeHidden();
    await expect(classic.locator("#conversation-title")).toContainText(
      other.name,
    );
    await classic
      .getByRole("button", { name: "Back to chats", exact: true })
      .click();
    await expect(
      classic
        .locator("#sidebar .chat-row")
        .filter({ hasText: other.name })
        .first(),
    ).toBeFocused();
    await classic.close();
    const combined = await browser.newPage({
      viewport: { width: 390, height: 844 },
    });
    await combined.route("https://offline.fixture.ts.net/**", (route) =>
      route.abort(),
    );
    await combined.addInitScript(() => {
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([
          {
            id: "offline",
            label: "Offline fixture",
            alias: "OFF",
            origin: "https://offline.fixture.ts.net",
            credentialId: "fixture-only",
          },
        ]),
      );
    });
    await combined.goto(origin);
    const combinedBoard = combined.locator("#sidebar");
    await expect(combined.locator(".server-views")).toHaveCount(1);
    await expect(combinedBoard).toBeInViewport({ ratio: 1 });
    const combinedStrip = combinedBoard
      .locator(".sidebar-project .chat-row")
      .filter({ hasText: lead.name })
      .first();
    await combinedStrip.click();
    await expect(combinedBoard).toBeHidden();
    await expect(combined.locator("#conversation-title")).toContainText(
      lead.name,
    );
    await combined
      .getByRole("button", { name: "Back to chats", exact: true })
      .click();
    await expect(combinedBoard).toBeInViewport({ ratio: 1 });
    await expect(combined.locator(".server-views")).toHaveAttribute(
      "inert",
      "",
    );
    await expect(
      combined.getByRole("button", { name: "Chat actions", exact: true }),
    ).toHaveCount(0);
    await expect(
      combinedBoard.locator(".chat-row").filter({ hasText: lead.name }).first(),
    ).toBeFocused();
    await combinedStrip.click();
    await expect(combinedBoard).toBeHidden();
    await expect(combined.locator(".server-views")).not.toHaveAttribute(
      "inert",
      "",
    );
    await combined.close();
    const desktop = await browser.newPage({
      viewport: { width: 1440, height: 900 },
    });
    await desktop.addInitScript(
      ({ stateDir, id }) =>
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        ),
      { stateDir: snapshot.stateDir, id: lead.id },
    );
    await desktop.goto(origin);
    await expect(desktop.locator("#conversation-title")).toContainText(
      lead.name,
    );
    await expect(
      desktop.getByRole("button", {
        name: "Toggle conversations",
        exact: true,
      }),
    ).toBeVisible();
    await desktop.reload();
    await expect(desktop.locator("#conversation-title")).toContainText(
      lead.name,
    );
    await desktop.close();
    console.log(
      JSON.stringify({
        evidence,
        widths: [320, 390, 1440],
        themes: ["light", "dark"],
      }),
    );
  } finally {
    fixture.kill("SIGTERM");
  }
});
