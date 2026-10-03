import { test } from "../playwright.mjs";
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { spawnFixture as spawn } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Team panel ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const repo = fileURLToPath(new URL("../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-team-panel-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await (await fetch(origin + "/api/state")).json();
    const diskApi = await (await fetch(origin + "/api/worktree-disk")).json();
    assert.equal(typeof diskApi.limitBytes, "number");
    assert.equal(typeof diskApi.totalBytes, "number");
    const page = runnerPage;
    await page.setViewportSize({ width: 1440, height: 960 });
    page.setDefaultTimeout(12000);
    const errors = [],
      actions = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/action", (route) => {
      actions.push(route.request().postDataJSON());
      return route.fulfill({
        json: {
          receipt: { requestId: route.request().postDataJSON().request_id },
          outcome: { status: "acknowledged" },
        },
      });
    });
    const lead = snapshot.threads.find(
      (agent) => agent.name === "Release lead",
    );
    const member = snapshot.threads.find(
      (agent) => agent.rootId === lead.id && !agent.isLead,
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
    const active = {
      ...member,
      id: "worker-active",
      name: "Active worker",
      status: "running",
      inFlight: true,
      worktree: false,
      error: undefined,
    };
    const needsYou = {
      ...member,
      id: "worker-answer",
      name: "Needs you",
      status: "approval",
      inFlight: false,
      worktree: false,
      error: undefined,
    };
    snapshot.threads = [lead, member, active, needsYou];
    snapshot.runtime.agents = [lead, member, active, needsYou];
    snapshot.runtime.requests = [
      { id: "active-answer", agent: active.id, status: "pending" },
      { id: "answer", agent: needsYou.id, status: "pending" },
    ];
    await page.route("**/api/worktree-disk**", (route) =>
      route.fulfill({
        json: {
          workers: {
            [member.id]: {
              state: "ready",
              bytes: 1024 ** 3,
              measure: "private on APFS",
            },
          },
          totalBytes: 1024 ** 3,
          limitBytes: 1024 ** 3,
          measure: "private on APFS",
          warning: true,
          scanning: false,
        },
      }),
    );
    await page.route("**/api/sync/**", (route) =>
      route.fulfill({
        status: 503,
        json: { error: "Fixture uses HTTP snapshots" },
      }),
    );
    await page.route(/\/api\/state(?:\?.*)?$/, (route) =>
      route.fulfill({ json: snapshot }),
    );
    await page.goto(origin);
    await page.locator("#message").waitFor();
    for (const width of [1440, 320]) {
      await page.setViewportSize({ width, height: 960 });
      await page.emulateMedia({
        colorScheme: width === 320 ? "dark" : "light",
      });
      await page.waitForFunction(
        (expectedWidth) =>
          window.innerWidth === expectedWidth &&
          document
            .querySelector("#team-toggle")
            ?.getAttribute("aria-expanded") === "false",
        width,
      );
      const priorityRequest =
        width === 1440
          ? page.waitForResponse((response) => {
              const url = new URL(response.url());
              const requested =
                url.searchParams.get("workers")?.split(",") || [];
              return (
                url.pathname === "/api/worktree-disk" &&
                [member.id, active.id, needsYou.id].every((id) =>
                  requested.includes(id),
                )
              );
            })
          : undefined;
      if (width <= 760) {
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page.getByRole("menuitem", { name: "Team", exact: true }).click();
      } else await page.locator("#team-toggle").click();
      if (priorityRequest) await priorityRequest;
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
        await panel
          .locator(".team-overview, #worker-search, .team-filters")
          .count(),
        0,
      );
      try {
        await panel.locator(".team-heading").waitFor();
      } catch (error) {
        const state = await page.evaluate(() => ({
          teamCount: document.querySelectorAll("#team").length,
          teams: [...document.querySelectorAll("#team")].map((team) => ({
            text: team.innerText,
            html: team.outerHTML.slice(0, 1200),
            visible: team.getBoundingClientRect().width > 0,
          })),
          headings: document.querySelectorAll(".team-heading").length,
          viewport: window.innerWidth,
        }));
        throw Error(
          `Team heading missing at ${width}px: ${JSON.stringify(state)}. ${error}`,
        );
      }
      assert.match(
        await panel.locator(".team-heading").innerText(),
        /3 subagents\b/,
      );
      assert.deepEqual(
        await panel.locator(".worker-entry strong").allInnerTexts(),
        ["Active worker", "Needs you", "Подача уведомления"],
        "compact panel puts Working first and keeps Need you visible",
      );
      assert.match(
        await panel.locator(".worker-disk").innerText(),
        /Disk: 1.0 GiB/,
      );
      assert.match(
        await panel.locator(".worker-disk").getAttribute("title"),
        /private on APFS/,
      );
      // The measure method stays in the tooltip, not in the visible label.
      assert.doesNotMatch(
        await panel.locator(".team-disk-total").innerText(),
        /APFS|allocated blocks/,
      );
      assert.doesNotMatch(
        await panel.locator(".worker-disk").innerText(),
        /APFS/,
      );
      assert.match(
        await panel.locator(".team-disk-total").innerText(),
        /Disk limit reached/,
      );
      assert.equal(
        await panel.locator(".worker-error").innerText(),
        "Could not prepare the project folder.",
      );
      const card = panel
        .locator(".worker-entry")
        .filter({ has: page.locator(`[data-worker="${member.id}"]`) });
      assert.ok((await card.boundingBox()).height < 250, "compact error card");
      assert.equal(await card.locator("pre").isVisible(), false);
      await panel.screenshot({ path: join(evidence, `team-${width}.png`) });
      await card.locator(".worker-error-details summary").click();
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
      await card.locator(".worker-error-details summary").click();
    }
    assert.deepEqual(errors, []);
    console.log(
      `PASS compact team, readable error, complete details, 1440/320px. ${evidence}`,
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    fixture.kill("SIGTERM");
  }
});
