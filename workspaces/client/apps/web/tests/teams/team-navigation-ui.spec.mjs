import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
} from "../playwright.mjs";
// Production client and an isolated fixture. No model calls or user state.
import assert from "node:assert/strict";
import { mkdir, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test.use({ apiSchemaFixtureHeaders: false, hasTouch: true });

test("team-navigation-ui", async ({ page: fixturePage }) => {
  test.setTimeout(60_000);
  const root = await mkdtemp(join(tmpdir(), "codex-team-navigation-"));
  const evidence = process.env.TEAM_NAVIGATION_EVIDENCE || root;
  await mkdir(evidence, { recursive: true });
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_BOARD_STATE_DIR: join(root, "board"),
      FIXTURE_WORKER_COUNT: "40",
      FIXTURE_AGENT_STEPS: JSON.stringify({
        "Worker 02": [
          {
            say: "Which scope should I review?",
            item: "team-navigation-answer",
            extra: {
              questions: [
                { title: "Which scope?", options: ["One file", "All files"] },
              ],
            },
          },
        ],
      }),
    },
  });
  let page,
    log = "";
  proc.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      const timeout = setTimeout(
        () => reject(Error("Fixture timeout\n" + log)),
        30000,
      );
      proc.stdout.once("data", (data) => {
        clearTimeout(timeout);
        resolve(Number(String(data).trim()));
      });
      proc.once("exit", () => {
        clearTimeout(timeout);
        reject(Error(log));
      });
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await readTestState(origin);
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const workers = initial.threads.filter(
      (agent) => agent.rootId === lead.id && !agent.isLead,
    );
    assert.equal(workers.length, 40);
    assert.equal(
      initial.runtime.agents.some(
        (agent) => agent.parentId === lead.id && agent.role === "reviewer",
      ),
      false,
      "navigation workers must persist after result delivery; reviewers auto-archive",
    );
    const worker = (n) =>
      workers.find(
        (agent) => agent.name === `Worker ${String(n).padStart(2, "0")}`,
      );
    const longName =
      "Check approval and interrupted worker navigation across a large release team";
    page = fixturePage;
    await fixturePage.setViewportSize({ width: 1440, height: 980 });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/sync/pull?*", async (route) => {
      const url = new URL(route.request().url());
      if (url.searchParams.get("scope") !== "state:entities:v1")
        return route.fallback();
      const response = await route.fetch();
      const data = await response.json();
      if (data.reset || !data.documents?.length)
        return route.fulfill({ response, json: data });
      // The server seeds idle workers; the roster's mix of states is scripted here.
      const desiredStatus = new Map(
        workers.map((agent, index) => [
          worker(index).id,
          { 3: "failed", 4: "paused", 7: "failed" }[index] ??
            (index < 8 ? "running" : index < 26 ? "queued" : "completed"),
        ]),
      );
      for (const document of data.documents) {
        if (document._deleted) continue;
        const payload = JSON.parse(document.payload);
        if (payload.collection === "agent" && payload.id === worker(2).id) {
          payload.value.name = longName;
          payload.value.status = "approval";
          payload.value.inFlight = false;
          document.payload = JSON.stringify(payload);
        } else if (
          payload.collection === "agent" &&
          desiredStatus.has(payload.id)
        ) {
          payload.value.status = desiredStatus.get(payload.id);
          payload.value.inFlight = payload.value.status === "running";
          document.payload = JSON.stringify(payload);
        }
      }
      // Only presentation values change. The real pending question is seeded
      // through the provider; server-owned sequences and checkpoints stay intact.
      await route.fulfill({ response, json: data });
    });
    await page.goto(origin);
    await expect(
      page.getByRole("alertdialog", {
        name: "Studio update required",
        exact: true,
      }),
    ).toHaveCount(0, { timeout: 2000 });
    const selectLead = () =>
      page.locator(`.project-chats [data-chat="${lead.id}"]`).click();
    await selectLead();
    assert.equal(
      await page.locator("#team-toggle").getAttribute("aria-expanded"),
      "false",
    );
    await page.locator("#team-toggle").click();
    const team = page.getByRole("complementary", {
      name: "Team",
      exact: true,
    });
    const search = team.getByRole("searchbox", { name: "Find a subagent" });
    const row = (n) => team.locator(`[data-worker="${worker(n).id}"]`);
    const openTeamFromHeader = async () => {
      await page
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await page.getByRole("menuitem", { name: "Team", exact: true }).click();
    };
    const openChatSettingsFromHeader = async () => {
      await page
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await page
        .getByRole("menuitem", { name: "Chat settings", exact: true })
        .click();
    };
    const waitForTeamInViewport = () =>
      page.waitForFunction(() => {
        const panel = document.querySelector("#team");
        const rect = panel?.getBoundingClientRect();
        return !!rect && rect.left >= 0 && rect.right <= innerWidth;
      });
    const waitForChatActionsHitTarget = async () => {
      try {
        await page.waitForFunction(
          () =>
            new Promise((resolve) => {
              let previous = "";
              let stableFrames = 0;
              const sample = () => {
                const button = document.querySelector(
                  'button[aria-label="Chat actions"]',
                );
                if (!button) {
                  stableFrames = 0;
                  requestAnimationFrame(sample);
                  return;
                }
                const rect = button.getBoundingClientRect();
                const target = document.elementFromPoint(
                  rect.left + rect.width / 2,
                  rect.top + rect.height / 2,
                );
                const hit = target === button || button.contains(target);
                const values = [
                  rect.x,
                  rect.y,
                  rect.width,
                  rect.height,
                  button.getAttribute("aria-expanded"),
                ].join(":");
                stableFrames =
                  hit && values === previous ? stableFrames + 1 : 0;
                previous = values;
                if (stableFrames >= 3) resolve(true);
                else requestAnimationFrame(sample);
              };
              requestAnimationFrame(sample);
            }),
        );
      } catch (error) {
        const state = await page.evaluate(() => {
          const button = document.querySelector(
            'button[aria-label="Chat actions"]',
          );
          if (!button) return { button: null };
          const rect = button.getBoundingClientRect();
          const target = document.elementFromPoint(
            rect.left + rect.width / 2,
            rect.top + rect.height / 2,
          );
          return {
            viewport: { width: innerWidth, height: innerHeight },
            chatActions: {
              rect: {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
              },
              expanded: button.getAttribute("aria-expanded"),
            },
            hitTarget: target && {
              tag: target.tagName,
              id: target.id,
              className:
                typeof target.className === "string" ? target.className : "",
            },
            drawers: [
              ...document.querySelectorAll(
                ".mantine-Drawer-root, .mantine-Drawer-content, .mantine-Drawer-overlay",
              ),
            ].map((node) => ({
              className: node.className,
              display: getComputedStyle(node).display,
              visibility: getComputedStyle(node).visibility,
              opacity: getComputedStyle(node).opacity,
              pointerEvents: getComputedStyle(node).pointerEvents,
            })),
          };
        });
        throw new Error(
          `Chat Actions hit target did not settle: ${JSON.stringify(state)}`,
          { cause: error },
        );
      }
    };
    const groupIds = (name) =>
      team
        .getByRole("region", { name, exact: true })
        .locator("[data-worker]")
        .evaluateAll((nodes) => nodes.map((node) => node.dataset.worker));
    await expect.poll(() => groupIds("Needs you")).toEqual([worker(2).id]);
    assert.deepEqual(
      (await groupIds("Failed")).sort(),
      [worker(3).id, worker(7).id].sort(),
    );
    assert.deepEqual(
      (await groupIds("Working")).sort(),
      [0, 1, 5, 6, ...Array.from({ length: 18 }, (_, index) => index + 8)]
        .map((n) => worker(n).id)
        .sort(),
      JSON.stringify(
        (await groupIds("Working")).map(
          (id) => workers.find((agent) => agent.id === id)?.name,
        ),
      ),
    );
    assert.equal(
      await team.getByRole("region", { name: "Waiting", exact: true }).count(),
      0,
    );
    assert.deepEqual(
      await team
        .locator("#workers > .team-status-group")
        .evaluateAll((nodes) =>
          nodes.map((node) => node.getAttribute("aria-label")),
        ),
      ["Needs you", "Failed", "Working", "Stopped"],
    );
    assert.equal(
      await row(2).evaluate((node) =>
        node.closest(".worker-entry")?.getAttribute("data-studio-state"),
      ),
      "answer",
    );
    assert.equal(
      await team.locator(".worker-group").getAttribute("open"),
      null,
    );
    assert.equal(
      await row(39).isVisible(),
      false,
      "completed workers start collapsed",
    );
    await team.getByRole("button", { name: "Failed", exact: true }).click();
    assert.equal(await team.locator("[data-worker]").count(), 2);
    await search.fill("Worker 39");
    await row(39).waitFor({ state: "visible" });
    assert.equal(
      await team.locator("[data-worker]").count(),
      1,
      "search includes completed workers even from the Failed filter",
    );
    await row(39).click();
    assert.equal(await row(39).getAttribute("aria-current"), "page");
    await page.locator("#back-lead").click();
    assert.equal(
      await page.locator("#conversation-title").innerText(),
      "Release lead",
    );
    await search.fill("no matching worker");
    await team.getByText("No workers match your search.").waitFor();
    await search.fill("");
    assert.equal(
      await team
        .getByRole("button", { name: "Failed", exact: true })
        .getAttribute("aria-pressed"),
      "true",
    );
    await team.getByRole("button", { name: "Active", exact: true }).click();
    assert.deepEqual(
      (
        await team
          .locator("[data-worker]")
          .evaluateAll((nodes) => nodes.map((node) => node.dataset.worker))
      ).sort(),
      [0, 1, 5, 6, ...Array.from({ length: 18 }, (_, index) => index + 8)]
        .map((n) => worker(n).id)
        .sort(),
      "Active filters the same canonical Working lane counted by the summary",
    );
    assert.equal(await row(3).count(), 0, "interrupted workers are not active");
    await team.getByRole("button", { name: "All", exact: true }).click();
    await row(2).click();
    assert.equal(await row(2).getAttribute("aria-current"), "page");
    assert.equal(await page.locator("#back-lead").count(), 1);
    assert.equal(
      await page
        .getByRole("button", { name: "Back to main agent", exact: true })
        .count(),
      2,
      "the header and Team use the same return label",
    );
    // Wider navigation can move header settings into the existing actions menu.
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await expect(
      page.getByRole("menuitem", { name: "Chat settings", exact: true }),
    ).toBeVisible();
    await page.keyboard.press("Escape");
    const title = row(2).locator("strong");
    const titleLayout = await title.evaluate((node) => ({
      text: node.textContent,
      clipped: node.scrollWidth > node.clientWidth,
      height: node.getBoundingClientRect().height,
      lineHeight: parseFloat(getComputedStyle(node).lineHeight),
    }));
    assert.equal(titleLayout.text, longName);
    assert.equal(
      titleLayout.clipped,
      true,
      "the strip keeps long names on one line",
    );
    assert.equal(
      await title.getAttribute("title"),
      longName,
      "the full name remains available",
    );
    assert.ok(
      titleLayout.height <= titleLayout.lineHeight * 1.5,
      "long names preserve dispatch density",
    );
    await team.locator("#workers").evaluate((node) => {
      node.scrollTop = node.scrollHeight;
    });
    assert.equal(
      await search.isVisible(),
      true,
      "search stays outside the worker scroll area",
    );
    await search.fill("Worker 39");
    await row(39).waitFor({ state: "visible" });
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    assert.equal(await team.count(), 0, "another chat cannot show this team");
    await selectLead();
    assert.equal(
      await search.inputValue(),
      "",
      "changing the chat clears its worker search",
    );
    assert.equal(
      await team
        .getByRole("button", { name: "All" })
        .getAttribute("aria-pressed"),
      "true",
    );
    await page.setViewportSize({ width: 800, height: 844 });
    await openTeamFromHeader();
    await search.waitFor({ state: "visible" });
    await search.fill("approval");
    await row(2).waitFor();
    assert.equal(
      await row(2).evaluate((node) => node.scrollWidth <= node.clientWidth),
      true,
    );
    await waitForTeamInViewport();
    await row(2).click();
    await team.waitFor({ state: "hidden" });
    await page.locator("#back-lead").click();
    assert.equal(
      await page.locator("#conversation-title").innerText(),
      "Release lead",
    );
    for (const width of [390, 320]) {
      await page.setViewportSize({ width, height: 844 });
      await page.waitForFunction(() => {
        if (!matchMedia("(max-width: 760px)").matches) return false;
        const toggle = document.querySelector("#sidebar-toggle");
        const sidebar = document.querySelector("#sidebar");
        if (!toggle) return false;
        const expanded = toggle.getAttribute("aria-expanded");
        const inDrawer = !!sidebar?.closest('[data-portal="true"]');
        return (
          (expanded === "false" && !sidebar) ||
          (expanded === "true" && inDrawer)
        );
      });
      const sidebarToggle = page.locator("#sidebar-toggle");
      if ((await sidebarToggle.getAttribute("aria-expanded")) === "true") {
        // Phone entry starts at the board; explicitly select the lead before typing.
        await selectLead();
        await page.waitForFunction(
          () =>
            document
              .querySelector("#sidebar-toggle")
              ?.getAttribute("aria-expanded") === "false" &&
            !document.querySelector("#sidebar"),
        );
      }
      await page.locator("#message").fill(`Lead draft ${width}`);
      await waitForChatActionsHitTarget();
      await openTeamFromHeader();
      await search.waitFor({ state: "visible" });
      await search.fill("Worker 39");
      await row(39).waitFor({ state: "visible" });
      await waitForTeamInViewport();
      const layout = await team.evaluate((n) => {
        const r = n.getBoundingClientRect();
        return {
          rect: { left: r.left, right: r.right, width: r.width },
          viewport: innerWidth,
          scrollWidth: n.scrollWidth,
          clientWidth: n.clientWidth,
          overflowingChildren: [...n.querySelectorAll("*")]
            .map((child) => {
              const box = child.getBoundingClientRect();
              return {
                tag: child.tagName,
                className:
                  typeof child.className === "string" ? child.className : "",
                left: box.left,
                right: box.right,
                width: box.width,
                scrollWidth: child.scrollWidth,
                clientWidth: child.clientWidth,
              };
            })
            .filter(
              (child) =>
                child.right > r.right + 0.5 || child.left < r.left - 0.5,
            )
            .slice(0, 8),
        };
      });
      assert.ok(
        layout.rect.left >= 0 &&
          layout.rect.right <= layout.viewport &&
          layout.scrollWidth <= layout.clientWidth,
        `the team list fits the mobile viewport: ${JSON.stringify(layout)}`,
      );
      await row(39).click();
      await team.waitFor({ state: "hidden" });
      await expect(page.locator("#conversation-title")).toHaveText("Worker 39");
      await expect(page.locator("#conversation-title")).toBeVisible();
      expect(
        await page
          .locator("#conversation-title")
          .evaluate((node) => node.getBoundingClientRect().width),
      ).toBeGreaterThanOrEqual(64);
      expect(
        await page.locator(".workspace-header").evaluate((header) => {
          const title = header
            .querySelector("#conversation-title")
            .getBoundingClientRect();
          return [...header.querySelectorAll("button, summary")].every(
            (control) => {
              const rect = control.getBoundingClientRect();
              return (
                rect.width === 0 ||
                rect.height === 0 ||
                rect.right <= title.left ||
                rect.left >= title.right ||
                rect.bottom <= title.top ||
                rect.top >= title.bottom
              );
            },
          );
        }),
        "the title does not overlap header controls",
      ).toBe(true);
      await page.screenshot({
        path: join(evidence, `worker-header-${width}.png`),
      });
      assert.ok(
        await page.locator("#back-lead").evaluate((n) => {
          const r = n.getBoundingClientRect();
          const header = n.closest("header").getBoundingClientRect();
          return r.top >= header.top && r.bottom <= header.bottom;
        }),
        "Back to main agent remains inside the mobile header",
      );
      await page.locator("#message").fill(`Worker draft ${width}`);
      await page.reload();
      // Reload deliberately returns phones to the board; reopen the worker explicitly.
      await expect(page.locator("#sidebar")).toBeVisible();
      await selectLead();
      await openTeamFromHeader();
      await search.fill("Worker 39");
      await row(39).click();
      await team.waitFor({ state: "hidden" });
      await page
        .locator("#conversation-title")
        .filter({ hasText: "Worker 39" })
        .waitFor();
      assert.equal(
        await page.locator("#message").inputValue(),
        `Worker draft ${width}`,
      );
      await openChatSettingsFromHeader();
      const chatSettings = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      await chatSettings
        .getByRole("tab", { name: "Models", exact: true })
        .click();
      const account = chatSettings.getByRole("group", {
        name: "Main agent account",
        exact: true,
      });
      await expect(account).toBeVisible();
      await expect(account).toHaveAttribute("aria-disabled", "true");
      await expect(account).not.toBeEmpty();
      await chatSettings
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await chatSettings.waitFor({ state: "hidden" });
      await page.locator("#back-lead").click();
      assert.equal(
        await page.locator("#message").inputValue(),
        `Lead draft ${width}`,
      );
      assert.ok(
        await page
          .locator(".workspace-header")
          .evaluate((n) => n.scrollWidth <= n.clientWidth),
      );
    }
    expect(errors).toEqual([]);
    console.log(
      JSON.stringify({
        passed: true,
        groups: true,
        filters: true,
        completedSearch: true,
        selection: true,
        chatIsolation: true,
        narrowDesktop: true,
        mobile: [320, 390],
        screenshots: evidence,
      }),
    );
  } catch (error) {
    try {
      await page?.screenshot({ path: join(evidence, "failure.png") });
    } catch {
      // Preserve the original test failure if Playwright already closed the page.
    }
    console.error("Failure evidence:", root);
    throw error;
  }
});
