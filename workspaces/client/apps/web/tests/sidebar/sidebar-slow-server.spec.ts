import { requiredFixtureValue } from "../servers/browser-fixture.ts";
import assert from "node:assert/strict";
import type {
  HeldRequest,
  Hold,
  SidebarParityFixture,
} from "./sidebar-parity-fixture.ts";
declare global {
  interface Window {
    sidebarSlowHistory: string[];
  }
}
import type { BrowserContext, Page, Locator } from "playwright";
import { test, expect } from "../playwright.ts";
import { sidebarParityFixture, sourcePath } from "./sidebar-parity-fixture.ts";

// A loaded backend answers late, and its answers can arrive out of order. The window
// must keep each change of the user until the server confirms it, must never show an
// older state after a newer one, and must not report a slow answer as a failure.

const delays = [2000, 10000, 30000];
const button = (scope: Locator, name: string) =>
  scope.getByRole("button", { name, exact: true });

// While `state.slow` is set, the chosen writes wait `delay` milliseconds before the
// server reads them. Entity pulls are answered at once but delivered late in turn, so
// an older answer can arrive after a newer one.
function slowServer(
  state: { slow: boolean },
  delay: number,
  write: (request: HeldRequest) => boolean = () => false,
): Hold {
  let pulls = 0;
  return (request: HeldRequest) => {
    if (!state.slow) return 0;
    if (
      request.method === "GET" &&
      request.path === "/api/sync/pull" &&
      request.url.searchParams.get("scope") === "state:entities:v1"
    )
      return { deliver: pulls++ % 2 ? 50 : delay };
    return write(request) ? delay : 0;
  };
}

async function setup(context: BrowserContext, page: Page, hold?: Hold) {
  test.setTimeout(300000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  return sidebarParityFixture(context, { hold });
}

// Another chat changes on the server. Each change sends a new sidebar snapshot from the
// server frame to the window.
let changes = 0;
function changeOtherChats(fixture: SidebarParityFixture) {
  changes++;
  for (const server of [fixture.local, fixture.remote] as const) {
    const row = server.snapshot.threads.find((chat) => chat.id === "root-a");
    assert.ok(row);
    row.tail = `Unrelated change ${changes}`;
    row.updated = Date.now() / 1000;
    server.complete(`unrelated-${changes}`);
  }
}

async function keepChanging(
  page: Page,
  fixture: SidebarParityFixture,
  until: number,
) {
  while (Date.now() < until) {
    await page.waitForTimeout(1500);
    changeOtherChats(fixture);
  }
}

// Record each distinct value that the page shows, at short intervals.
async function recordExpanded(page: Page, text: string) {
  await page.evaluate((text) => {
    const history: string[] = [];
    const read = () =>
      [...document.querySelectorAll("#sidebar [aria-expanded]")]
        .find((element) => element.textContent?.includes(text))
        ?.getAttribute("aria-expanded") ?? "missing";
    const tick = () => {
      const value = read();
      if (history.at(-1) !== value) history.push(value);
    };
    tick();
    window.sidebarSlowHistory = history;
    setInterval(tick, 20);
  }, text);
}

async function recordProjectOrder(page: Page) {
  await page.evaluate(() => {
    const history: string[] = [];
    const tick = () => {
      const value = [
        ...document.querySelectorAll<HTMLElement>("#sidebar .sidebar-project"),
      ]
        .map((row) => row.dataset.projectPath)
        .join(" ");
      if (history.at(-1) !== value) history.push(value);
    };
    tick();
    window.sidebarSlowHistory = history;
    setInterval(tick, 20);
  });
}

const history = (page: Page) => page.evaluate(() => window.sidebarSlowHistory);

for (const delay of delays) {
  test(`a folder collapse never moves back while the server answers after ${delay} ms`, async ({
    page,
    context,
  }) => {
    const state = { slow: false };
    const fixture = await setup(
      context,
      page,
      slowServer(
        state,
        delay,
        ({ owner, method, path }) =>
          owner === "local" &&
          method === "POST" &&
          path === "/api/sync/preferences",
      ),
    );
    try {
      const sidebar = await fixture.open(page);
      const folder = sidebar
        .getByRole("button", { name: /Local Parent/ })
        .first();
      await expect(folder).toHaveAttribute("aria-expanded", "true");
      await recordExpanded(page, "Local Parent");
      state.slow = true;
      await folder.click();
      await expect(folder).toHaveAttribute("aria-expanded", "false");
      await keepChanging(page, fixture, Date.now() + delay + 3000);
      await expect
        .poll(
          () =>
            Object.entries(fixture.preferences.local.server).some(
              ([name, field]) =>
                name.includes('\\"parent\\"') && field.value === true,
            ),
          { timeout: 2 * delay + 10000 },
        )
        .toBe(true);
      // Late answers of earlier pulls still arrive after the server saved the change.
      await keepChanging(page, fixture, Date.now() + delay + 3000);
      expect(await history(page)).toEqual(["true", "false"]);
    } finally {
      await fixture.close();
    }
  });

  test(`a project move never moves back while the server answers after ${delay} ms`, async ({
    page,
    context,
  }) => {
    const state = { slow: false };
    const fixture = await setup(
      context,
      page,
      slowServer(
        state,
        delay,
        ({ owner, method, path, body }) =>
          owner === "local" &&
          method === "POST" &&
          path === "/api/projects" &&
          body.action === "reorder",
      ),
    );
    const original = fixture.local.snapshot.runtime.projects[0];
    fixture.local.snapshot.runtime.projects.push({
      ...structuredClone(original),
      id: "other-project",
      name: "Other project",
      path: "/same/other",
      folders: [],
      peerTeams: [],
    });
    requiredFixtureValue(
      requiredFixtureValue(
        requiredFixtureValue(fixture.local.snapshot.runtime.sidebarOrder)
          .groups,
      ).projects,
    ).push("/same/other");
    try {
      const sidebar = await fixture.open(page);
      const otherProject = sidebar.locator(".project-tree-toggle", {
        hasText: "Other project",
      });
      await expect(otherProject).toBeVisible();
      const before = [
        JSON.stringify(["project", "local", "logical-project"]),
        JSON.stringify(["project", "local", "other-project"]),
      ].join(" ");
      const after = [
        JSON.stringify(["project", "local", "other-project"]),
        JSON.stringify(["project", "local", "logical-project"]),
      ].join(" ");
      await recordProjectOrder(page);
      state.slow = true;
      await otherProject.focus();
      await page.keyboard.press("Alt+ArrowUp");
      await keepChanging(page, fixture, Date.now() + delay + 3000);
      await expect
        .poll(
          () =>
            requiredFixtureValue(
              requiredFixtureValue(fixture.local.snapshot.runtime.sidebarOrder)
                .groups,
            ).projects,
          { timeout: 2 * delay + 10000 },
        )
        .toEqual(["/same/other", sourcePath("local")]);
      await keepChanging(page, fixture, Date.now() + delay + 3000);
      await expect
        .poll(
          () =>
            page.evaluate(
              () =>
                !Object.keys(localStorage).some(
                  (key) =>
                    key.startsWith("codex-sidebar-order:") &&
                    key.endsWith(":pending"),
                ),
            ),
          { timeout: 2 * delay + 10000 },
        )
        .toBe(true);
      expect(await history(page)).toEqual([before, after]);
      // A slow answer is not an error.
      await expect(page.getByText(/Could not save sidebar order/)).toHaveCount(
        0,
      );
    } finally {
      await fixture.close();
    }
  });

  test(`the new chat dialog keeps its choices while the server answers after ${delay} ms`, async ({
    page,
    context,
  }) => {
    const state = { slow: false };
    const fixture = await setup(
      context,
      page,
      slowServer(
        state,
        delay,
        ({ owner, method, path }) =>
          owner === "local" &&
          method === "GET" &&
          (path === "/api/models" || path === "/api/projects"),
      ),
    );
    // Another window collapses a folder. This window reads the new preference, and
    // rereads its saved server list.
    let edits = 0;
    const editElsewhere = () => {
      edits++;
      fixture.preferences.local.server[
        JSON.stringify([
          "codex-project-tree:/same/state",
          JSON.stringify([sourcePath("local"), "folder", "empty"]),
        ])
      ] = {
        value: edits % 2 === 1,
        timestamp: Date.now(),
        writer: "other-window",
      };
      changeOtherChats(fixture);
    };
    try {
      const sidebar = await fixture.open(page);
      state.slow = true;
      await sidebar
        .getByRole("button", { name: /^Local Empty/ })
        .first()
        .hover();
      await button(sidebar, "New chat in folder Local Empty").click();
      const dialog = page.getByRole("dialog", {
        name: "New chat",
        exact: true,
      });
      const start = button(dialog, "Start chat");
      const worktree = dialog.getByRole("radio", {
        name: "worktree",
        exact: true,
      });
      // The reads finish although other changes arrive while they wait. Two reads follow
      // each other, and the browser queues requests behind its few open connections.
      const loaded = Date.now() + 3 * delay + 30000;
      while (!(await start.isEnabled()) && Date.now() < loaded) {
        editElsewhere();
        await page.waitForTimeout(3000);
      }
      await expect(start).toBeEnabled();
      await dialog.getByText("worktree", { exact: true }).click();
      await expect(worktree).toBeChecked();
      for (let step = 0; step < 3; step++) {
        editElsewhere();
        await page.waitForTimeout(1500);
        await expect(worktree).toBeChecked();
        await expect(start).toBeEnabled();
      }
      await start.click();
      await expect
        .poll(
          () =>
            fixture.calls.local.find((call) => call.path === "/api/leads")
              ?.body,
        )
        .toMatchObject({ workspaceMode: "worktree" });
    } finally {
      await fixture.close();
    }
  });

  test(`a chat creation that the server answers after ${delay} ms opens the chat`, async ({
    page,
    context,
  }) => {
    const state = { slow: false };
    const fixture = await setup(
      context,
      page,
      slowServer(
        state,
        delay,
        ({ owner, method, path }) =>
          owner === "local" && method === "POST" && path === "/api/leads",
      ),
    );
    try {
      const sidebar = await fixture.open(page);
      const chats = fixture.local.snapshot.threads.length;
      await sidebar
        .getByRole("button", { name: /^Local Empty/ })
        .first()
        .hover();
      await button(sidebar, "New chat in folder Local Empty").click();
      const dialog = page.getByRole("dialog", {
        name: "New chat",
        exact: true,
      });
      const start = button(dialog, "Start chat");
      await expect(start).toBeEnabled();
      state.slow = true;
      await start.click();
      const local = page.frameLocator('iframe[title="Studio on Local"]');
      const notice = local.getByText(
        /Studio lost confirmation for this chat request/,
      );
      const created = sidebar
        .getByRole("button", { name: /local new chat/ })
        .first();
      // Look for a false failure until the chat opens.
      const deadline = Date.now() + 2 * delay + 30000;
      while (!(await created.isVisible()) && Date.now() < deadline) {
        await expect(notice).toHaveCount(0);
        await page.waitForTimeout(500);
      }
      await expect(created).toBeVisible();
      await expect(notice).toHaveCount(0);
      expect(fixture.local.snapshot.threads.length).toBe(chats + 1);
      const requests = fixture.calls.local.filter(
        (call) => call.path === "/api/leads",
      );
      expect(requests.length).toBeGreaterThan(0);
      expect(
        new Set(requests.map((call) => JSON.stringify(call.body))).size,
      ).toBe(1);
    } finally {
      await fixture.close();
    }
  });
}

test("a lost chat creation answer still keeps its request for a retry", async ({
  page,
  context,
}) => {
  const fixture = await setup(context, page);
  try {
    const sidebar = await fixture.open(page);
    const chats = fixture.local.snapshot.threads.length;
    // The server creates the chat, but the answer does not reach the window.
    let lost = 0;
    await page.route("**/api/leads", async (route) => {
      const response = await route.fetch();
      expect(response.ok()).toBe(true);
      lost++;
      await route.abort("failed");
    });
    await sidebar
      .getByRole("button", { name: /^Local Empty/ })
      .first()
      .hover();
    await button(sidebar, "New chat in folder Local Empty").click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await expect(button(dialog, "Start chat")).toBeEnabled();
    await button(dialog, "Start chat").click();
    const local = page.frameLocator('iframe[title="Studio on Local"]');
    const retry = local.getByRole("button", {
      name: "Retry chat request",
      exact: true,
    });
    await expect(retry).toBeVisible();
    expect(lost).toBe(1);
    await page.unroute("**/api/leads");
    await retry.click();
    await expect(retry).toHaveCount(0);
    expect(fixture.local.snapshot.threads.length).toBe(chats + 1);
    const requests = fixture.calls.local.filter(
      (call) => call.path === "/api/leads",
    );
    expect(requests).toHaveLength(2);
    expect(requiredFixtureValue(requests[1]).body).toEqual(
      requiredFixtureValue(requests[0]).body,
    );
  } finally {
    await fixture.close();
  }
});
