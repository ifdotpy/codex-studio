import type { TestAgent } from "../playwright.ts";
import { required, fixturePostBody } from "./helpers.ts";
import { waitForFixturePort } from "../playwright.ts";
import {
  readTestState,
  stubEntityState,
  readFixtureSyncContract,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
} from "../playwright.ts";
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Team panel ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const evidence = await mkdtemp(join(tmpdir(), "studio-team-panel-"));
  const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  fixture.stderr?.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await waitForFixturePort(fixture);
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await readTestState(origin);
    const page = runnerPage;
    const now = Date.now();
    await page.clock.setFixedTime(now);
    await page.setViewportSize({ width: 1440, height: 960 });
    page.setDefaultTimeout(12000);
    const errors: string[] = [],
      actions = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/action", (route) => {
      actions.push(fixturePostBody(route, "/api/action"));
      return route.fulfill({
        json: {
          receipt: {
            requestId: fixturePostBody(route, "/api/action").request_id,
          },
          outcome: { status: "acknowledged" },
        },
      });
    });
    const lead = required(
      snapshot.threads.find((agent) => agent.name === "Release lead"),
    );
    const member = required(
      snapshot.threads.find(
        (agent) => agent.rootId === lead.id && !agent.isLead,
      ),
    );
    const rawError =
      "Command ['git', '-C', '/workspace/project', 'worktree', 'add', '-b', 'codex-agent/" +
      "12345678-".repeat(8) +
      "', '/workspace/project/.worktrees/codex-agents/" +
      "12345678-".repeat(8) +
      "', 'HEAD'] returned non-zero exit status 128.";
    Object.assign(member, {
      name: "Подача уведомления",
      status: "failed",
      worktree: true,
      inFlight: false,
      error: rawError,
      overview: {
        task: "Research current official government sources for filing a notification.",
        result: "",
      },
    });
    const active: TestAgent = {
      ...member,
      id: "worker-active",
      name: "Active worker",
      status: "running",
      inFlight: true,
      commandProgress: [
        {
          monitorId: "fixture-gate",
          stage: "just test-fast",
          startedAt: now / 1000 - 720,
          lastOutputLine: "Rust checks passed; browser checks running",
        },
      ],
      worktree: false,
      error: undefined,
    };
    const needsYou: TestAgent = {
      ...member,
      id: "worker-answer",
      name: "Needs you",
      status: "approval",
      inFlight: false,
      worktree: false,
      error: undefined,
    };
    const stalled: TestAgent = {
      ...member,
      id: "worker-stalled",
      name: "Stalled worker",
      status: "completed",
      error: undefined,
      stall: { taskId: "task-14", since: now / 1000 - 300 },
    };
    lead.commandProgress = [
      {
        monitorId: "fixture-lead-gate",
        stage: "Final review",
        startedAt: now / 1000 - 60,
        lastOutputLine: "Reviewing changes",
      },
    ];
    snapshot.threads = [lead, member, active, needsYou, stalled];
    snapshot.runtime.agents = [lead, member, active, needsYou, stalled];
    snapshot.runtime.requests = [
      { id: "answer", agent: needsYou.id, status: "pending" },
    ];
    await stubEntityState(
      page,
      snapshot,
      await readFixtureSyncContract(origin),
    );
    await page.goto(origin);
    await page.locator("#message").waitFor();
    for (const width of [1440, 390, 320]) {
      await page.setViewportSize({ width, height: 960 });
      await page.emulateMedia({
        colorScheme: width <= 390 ? "dark" : "light",
      });
      await page.waitForFunction(
        (expectedWidth) =>
          window.innerWidth === expectedWidth &&
          document
            .querySelector("#team-toggle")
            ?.getAttribute("aria-expanded") === "false",
        width,
      );
      if (width <= 760) {
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page.getByRole("menuitem", { name: "Team", exact: true }).click();
      } else await page.locator("#team-toggle").click();
      await page.locator("#team").waitFor({ state: "visible" });
      const panel = page.locator("#team");
      await page.waitForFunction(() => {
        const team = document.querySelector("#team");
        const card = team?.querySelector(".worker-entry");
        const heading = team?.querySelector(".team-heading");
        if (
          !(team instanceof HTMLElement) ||
          !(card instanceof HTMLElement) ||
          !(heading instanceof HTMLElement)
        )
          return false;
        const before = team.getBoundingClientRect().toJSON();
        return new Promise((resolve) =>
          requestAnimationFrame(() =>
            requestAnimationFrame(() => {
              const after = team.getBoundingClientRect();
              resolve(
                team === document.querySelector("#team") &&
                  team.contains(card) &&
                  team.contains(heading) &&
                  team.querySelector(".worker-entry") === card &&
                  team.querySelector(".team-heading") === heading &&
                  before.x === after.x &&
                  before.y === after.y &&
                  before.width === after.width &&
                  before.height === after.height,
              );
            }),
          ),
        );
      });
      assert.equal(
        await panel.locator("#worker-search, .team-summary-filters").count(),
        2,
      );
      assert.equal(await panel.locator(".team-overview").count(), 1);
      await expect(panel.locator("[data-lead]")).toHaveCount(0);
      await expect(panel.locator('[data-team-count="all"] strong')).toHaveText(
        "4",
      );
      const geometry = await panel.evaluate((team) => {
        const style = getComputedStyle(team);
        const probe = document.createElement("div");
        probe.style.width = "var(--mantine-spacing-md)";
        team.append(probe);
        const gutter = probe.getBoundingClientRect().width;
        probe.style.width = "var(--mantine-spacing-sm)";
        probe.style.height = "var(--mantine-spacing-xs)";
        const { width: inlinePadding, height: blockPadding } =
          probe.getBoundingClientRect();
        probe.remove();
        return {
          gutter,
          inlinePadding,
          blockPadding,
          strips: [...team.querySelectorAll(".worker-entry")].map((strip) => {
            const computed = getComputedStyle(strip);
            return {
              inline: parseFloat(computed.paddingInlineStart),
              top: parseFloat(computed.paddingBlockStart),
              bottom: parseFloat(computed.paddingBlockEnd),
            };
          }),
          padding: parseFloat(style.paddingInlineStart),
          edge:
            team.getBoundingClientRect().left +
            parseFloat(style.borderInlineStartWidth) +
            gutter,
          edges: [
            ...team.querySelectorAll(
              ".team-heading h2, .team-summary-filters, :scope > .mantine-TextInput-root, .team-status-group h3, .worker-entry:not([data-has-parent])",
            ),
          ].map((node) => node.getBoundingClientRect().left),
        };
      });
      assert.ok(
        Math.abs(geometry.padding - geometry.gutter) < 1,
        "Team uses the shared md gutter",
      );
      for (const strip of geometry.strips) {
        assert.ok(
          strip.inline >= geometry.inlinePadding,
          "Strip content has at least sm inline padding",
        );
        assert.ok(
          strip.top >= geometry.blockPadding &&
            strip.bottom >= geometry.blockPadding,
          "Strip content has at least xs block padding",
        );
      }
      for (const edge of geometry.edges)
        assert.ok(
          Math.abs(edge - geometry.edge) < 1,
          "Team content and top-level rails share the inner edge",
        );
      try {
        await panel.locator(".team-heading").waitFor();
      } catch (error) {
        const state = await page.evaluate(() => ({
          teamCount: document.querySelectorAll("#team").length,
          teams: [...document.querySelectorAll<HTMLElement>("#team")].map(
            (team) => ({
              text: team.innerText,
              html: team.outerHTML.slice(0, 1200),
              visible: team.getBoundingClientRect().width > 0,
            }),
          ),
          headings: document.querySelectorAll(".team-heading").length,
          viewport: window.innerWidth,
        }));
        throw Error(
          `Team heading missing at ${width}px: ${JSON.stringify(state)}. ${error}`,
        );
      }
      assert.equal(await panel.locator(".team-heading h2").innerText(), "Team");
      assert.equal(
        await panel.locator('[data-team-count="working"] strong').innerText(),
        "1",
      );
      assert.equal(
        await panel.locator('[data-team-count="answer"] strong').innerText(),
        "1",
      );
      assert.equal(
        await panel.locator('[data-team-count="error"] strong').innerText(),
        "1",
      );
      await expect(panel.locator(".worker-entry strong")).toHaveText([
        "Needs you",
        "Подача уведомления",
        "Active worker",
        "Stalled worker",
      ]);
      assert.equal(
        await panel.locator('[data-team-count="unread"] strong').innerText(),
        "1",
      );
      await panel
        .locator(".worker-entry")
        .filter({ has: page.locator('[data-worker="worker-active"]') })
        .locator(".worker-command-details > .disclosure-control")
        .click();
      const command = panel.locator('[data-monitor="fixture-gate"]');
      await expect(command).toContainText(/just test-fast · 12m/);
      await expect(command).toContainText(
        "Rust checks passed; browser checks running",
      );
      await expect(panel.locator(".agent-stall")).toHaveText(
        "Waiting for lead",
      );
      if (width > 760) {
        const leadRow = page.locator(`.project-chats [data-chat="${lead.id}"]`);
        await expect(leadRow).toBeVisible();
        // Sidebar strips reserve space for titles; routine command progress stays in Team.
        await expect(leadRow.locator("[data-monitor]")).toHaveCount(0);
      }
      assert.equal(
        await panel.locator(".worker-disk, .team-disk-total").count(),
        0,
      );
      await expect(panel.locator(".worker-error")).toHaveText(
        "Could not prepare the project folder.",
      );
      const card = panel
        .locator(".worker-entry")
        .filter({ has: page.locator(`[data-worker="${member.id}"]`) });
      assert.ok(
        required(await card.boundingBox()).height < 250,
        "compact error card",
      );
      assert.equal(await card.locator("pre").isVisible(), false);
      await panel.screenshot({ path: join(evidence, `team-${width}.png`) });
      await card.locator(".worker-error-details > .disclosure-control").click();
      assert.equal(
        await card.locator("pre").innerText(),
        rawError,
        "full original error remains available",
      );
      assert.ok(await card.locator("pre").isVisible());
      assert.ok(
        await panel.evaluate(
          (node) => node.scrollWidth <= node.clientWidth + 1,
        ),
        "no horizontal overflow",
      );
      await card.locator(".worker-error-details > .disclosure-control").click();
      await panel.locator("#team-close").click();
      await panel.waitFor({ state: "hidden" });
    }
    assert.deepEqual(errors, []);
    console.log(
      `PASS compact team, readable error, complete details, 1440/390/320px. ${evidence}`,
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
  }
});

test("Team panel scrolls past retained worktrees to the last worker", async ({
  page,
}) => {
  test.setTimeout(120_000);
  page.setDefaultTimeout(10_000);
  const root = await mkdtemp(join(tmpdir(), "studio-team-scroll-"));
  const fixture = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
  let log = "";
  fixture.stderr?.on("data", (chunk) => {
    log += chunk;
  });
  const port = await waitForFixturePort(fixture);
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await readTestState(origin);
  const lead = required(
    snapshot.threads.find((agent) => agent.name === "Release lead"),
  );
  const member = required(
    snapshot.threads.find((agent) => agent.rootId === lead.id),
  );
  const workers = Array.from({ length: 24 }, (_, index): TestAgent => ({
    ...member,
    id: `scroll-worker-${index}`,
    name: `Scroll worker ${String(index).padStart(2, "0")}`,
    status: "running",
    inFlight: true,
  }));
  snapshot.threads = [lead, ...workers];
  snapshot.runtime.agents = snapshot.threads;
  await stubEntityState(page, snapshot, await readFixtureSyncContract(origin));
  await page.route("**/api/retained-worktrees?*", (route) =>
    route.fulfill({
      json: {
        retainedWorktrees: workers.map((_, index) => ({
          id: `retained-${index}`,
          name: `Deleted worker ${index}`,
          cleanup: {
            phase: "not_sent",
            reason: "Worktree contains uncommitted changes.",
            path: `/workspace/retained-${index}`,
          },
        })),
      },
    }),
  );
  await page.setViewportSize({ width: 1440, height: 720 });
  await page.goto(origin);
  await expect(page.locator("#message")).toBeVisible();
  for (const width of [1440, 1024, 390, 320]) {
    await page.setViewportSize({ width, height: 720 });
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Team", exact: true }).click();
    const panel = page.locator("#team");
    await expect(panel).toBeInViewport({ ratio: 1 });
    await expect(panel.locator(".retained-worktree")).toHaveCount(24);
    await expect(panel.locator(".worker-entry")).toHaveCount(24);
    const lastWorker = panel.locator(".worker-entry").last();
    await expect(lastWorker).not.toBeInViewport();
    const box = required(await panel.boundingBox());
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.wheel(0, 100_000);
    await expect
      .poll(() => panel.evaluate((node) => node.scrollTop))
      .toBeGreaterThan(0);
    // content-visibility measures the offscreen rows as they enter view.
    await lastWorker.scrollIntoViewIfNeeded();
    await expect(lastWorker).toBeInViewport({ ratio: 1 });
    const lastRetained = panel.locator(".retained-worktree").last();
    await lastRetained.scrollIntoViewIfNeeded();
    await expect(lastRetained).toBeInViewport({ ratio: 1 });
    expect(
      await panel.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
    ).toBe(true);
    // The same close control remains reachable after navigating the long list.
    await panel.locator("#team-close").scrollIntoViewIfNeeded();
    await panel.locator("#team-close").click();
    await expect(panel).toBeHidden();
  }
});
