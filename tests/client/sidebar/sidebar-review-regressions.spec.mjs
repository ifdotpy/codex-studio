import { test, expect } from "../playwright.mjs";
import {
  sidebarParityFixture,
  sourcePath,
  scope,
} from "./sidebar-parity-fixture.mjs";
test("selecting a remote chat expands only the session view and explicit heading edits use home storage", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(90000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const f = await sidebarParityFixture(context);
  await context.addInitScript(
    ({ scope }) => {
      const owner =
        new URLSearchParams(location.search).get("studio-server") === "remote"
          ? "remote"
          : "local";
      const prefix = owner === "remote" ? ":server:remote:" : "";
      const key = prefix + "codex-project-tree:" + scope;
      const value = JSON.parse(localStorage.getItem(key) || "{}");
      value[owner === "remote" ? "/remote/project" : "/same/project"] = true;
      localStorage.setItem(key, JSON.stringify(value));
      window.__reviewWrites = [];
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function (k, v) {
        if (k.includes("codex-project-tree:"))
          window.__reviewWrites.push([k, v]);
        return original.call(this, k, v);
      };
    },
    { scope },
  );
  try {
    await page.goto(f.local.origin + "/?studio-navigation=combined");
    const sidebar = page.locator("#sidebar");
    const group = sidebar.getByRole("button", {
      name: "Home project",
      exact: true,
    });
    await expect(group).toHaveAttribute("aria-expanded", "false");
    await sidebar
      .getByLabel("Filter projects and chats")
      .fill("Remote Saved pin");
    await expect(
      sidebar
        .locator(".sidebar-row")
        .filter({ hasText: "Remote Saved pin" })
        .locator("[data-chat]")
        .first(),
    ).toBeVisible();
    await page.evaluate(() => {
      window.__reviewWrites = [];
    });
    await sidebar
      .locator(".sidebar-row")
      .filter({ hasText: "Remote Saved pin" })
      .locator("[data-chat]")
      .first()
      .click();
    await expect(group).toHaveAttribute("aria-expanded", "true");
    const result = await page.evaluate(
      ({ scope }) => ({
        writes: window.__reviewWrites,
        local: JSON.parse(
          localStorage.getItem("codex-project-tree:" + scope) || "{}",
        ),
        remote: JSON.parse(
          localStorage.getItem(":server:remote:codex-project-tree:" + scope) ||
            "{}",
        ),
      }),
      { scope },
    );
    await testInfo.attach("cross-writes", {
      body: JSON.stringify(result, null, 2),
      contentType: "application/json",
    });
    expect(result.local[sourcePath("local")]).toBe(true);
    expect(result.remote[sourcePath("remote")]).toBe(true);
    expect(result.writes).toEqual([]);
    await group.click();
    await group.click();
    const saved = await page.evaluate(
      ({ scope }) => ({
        local: JSON.parse(
          localStorage.getItem("codex-project-tree:" + scope) || "{}",
        ),
        remote: JSON.parse(
          localStorage.getItem(":server:remote:codex-project-tree:" + scope) ||
            "{}",
        ),
      }),
      { scope },
    );
    expect(saved.local[sourcePath("local")]).toBe(false);
    expect(saved.remote[sourcePath("remote")]).toBe(true);
  } finally {
    await f.close();
  }
});
test("a combined RPC reply excludes prompt and answer fields", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(90000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const f = await sidebarParityFixture(context);
  await context.addInitScript(() => {
    window.__reviewReplies = [];
    window.addEventListener("message", (event) => {
      if (event.data?.kind === "studio-sidebar-result")
        window.__reviewReplies.push(event.data);
    });
  });
  await context.route("**/api/organization", async (route) => {
    const body = route.request().postDataJSON();
    await route.fulfill({
      json: {
        ...f.remote.snapshot.threads.find((row) => row.id === body.id),
        prompt: "PRIVATE_PROMPT_SENTINEL",
        lastAnswer: "PRIVATE_ANSWER_SENTINEL",
        pinned: true,
      },
    });
  });
  try {
    const sidebar = await f.open(page);
    const row = sidebar
      .locator(".sidebar-row")
      .filter({ hasText: "Remote Root A" });
    await row.hover();
    await row
      .getByRole("button", { name: "Actions for Remote Root A", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
    await expect
      .poll(() => page.evaluate(() => window.__reviewReplies.length))
      .toBeGreaterThan(0);
    const replies = await page.evaluate(() => window.__reviewReplies);
    await testInfo.attach("raw-reply", {
      body: JSON.stringify(replies, null, 2),
      contentType: "application/json",
    });
    expect(JSON.stringify(replies)).not.toContain("PRIVATE_PROMPT_SENTINEL");
    expect(JSON.stringify(replies)).not.toContain("PRIVATE_ANSWER_SENTINEL");
  } finally {
    await f.close();
  }
});
test("combined frames do not mount sidebars or migrate null groups", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(60000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const f = await sidebarParityFixture(context);
  f.local.snapshot.runtime.sidebarOrder = { revision: 0, groups: null };
  f.remote.snapshot.runtime.sidebarOrder = { revision: 0, groups: null };
  try {
    await f.open(page);
    for (const title of ["This computer", "Remote"])
      await expect(
        page
          .frameLocator(`iframe[title="Studio on ${title}"]`)
          .locator("#sidebar"),
      ).toHaveCount(0);
    expect(f.calls.local.filter((call) => call.body.migration)).toEqual([]);
    expect(f.calls.remote.filter((call) => call.body.migration)).toEqual([]);
    await testInfo.attach("read-migrations", {
      body: JSON.stringify(f.calls, null, 2),
      contentType: "application/json",
    });
  } finally {
    await f.close();
  }
});
test("unregistered chat directories can collapse", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(60000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const f = await sidebarParityFixture(context);
  f.remote.snapshot.runtime.projects = [];
  f.remote.snapshot.runtime.peerTeams = [];
  try {
    const sidebar = await f.open(page);
    const group = sidebar.getByRole("button", {
      name: "/remote/project",
      exact: true,
    });
    await expect(group).toHaveAttribute("aria-expanded", "true");
    await group.click();
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    await expect(group).toHaveAttribute("aria-expanded", "false");
    await testInfo.attach("unregistered-collapse", {
      body: JSON.stringify({
        expanded: await group.getAttribute("aria-expanded"),
      }),
      contentType: "application/json",
    });
  } finally {
    await f.close();
  }
});

test("a pinned chat edge drop cannot join a team in classic", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(60000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const f = await sidebarParityFixture(context, { paired: false });
  try {
    const sidebar = await f.open(page, { combined: false });
    const from = sidebar
      .locator(".sidebar-row")
      .filter({ hasText: "Local Saved pin" })
      .locator("[data-chat]");
    const to = sidebar
      .locator(".peer-team-toggle")
      .filter({ hasText: "Local Saved team" });
    const drop = async (position) => {
      await from.scrollIntoViewIfNeeded();
      const a = await from.boundingBox();
      await page.mouse.move(a.x + 18, a.y + a.height / 2);
      await page.mouse.down();
      await page.mouse.move(a.x + 22, a.y + a.height / 2 + 9, { steps: 6 });
      await to.scrollIntoViewIfNeeded();
      const b = await to.boundingBox();
      await page.mouse.move(b.x + 18, b.y + b.height * position, { steps: 20 });
      await page.mouse.move(b.x + 20, b.y + b.height * position, { steps: 3 });
      await page.mouse.up();
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
    };
    for (const edge of [0.1, 0.9]) {
      await drop(edge);
      expect(
        f.calls.local.filter(
          (call) =>
            call.path === "/api/peer-teams" && call.body.action === "move",
        ),
      ).toHaveLength(0);
      expect(f.local.snapshot.runtime.peerTeams[0].members).not.toContain(
        "overlap",
      );
    }
    await testInfo.attach("edge-mutation", {
      body: JSON.stringify(f.calls.local, null, 2),
      contentType: "application/json",
    });
    await drop(0.5);
    await expect
      .poll(
        () =>
          f.calls.local.filter(
            (call) =>
              call.path === "/api/peer-teams" && call.body.action === "move",
          ).length,
      )
      .toBe(1);
    expect(f.local.snapshot.runtime.peerTeams[0].members).toContain("overlap");
  } finally {
    await f.close();
  }
});

test("classic null groups keep the existing source migration", async ({
  page,
  context,
}) => {
  test.setTimeout(60000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const f = await sidebarParityFixture(context, { paired: false });
  f.local.snapshot.runtime.sidebarOrder = { revision: 0, groups: null };
  try {
    await f.open(page, { combined: false });
    await expect
      .poll(() => f.calls.local.filter((call) => call.body.migration).length)
      .toBe(1);
  } finally {
    await f.close();
  }
});
