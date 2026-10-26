import assert from "node:assert/strict";
import type { PostBody } from "../../src/apiTypes.ts";
import { fixtureOrigin, fixturePostBody } from "../servers/browser-fixture.ts";
import type { Page } from "playwright";
import {
  test,
  expect,
  createFixtureViteServer,
  apiSchemaHeaders,
} from "../playwright.ts";
import { resolve } from "node:path";

let server: Awaited<ReturnType<typeof createFixtureViteServer>> | undefined;
let url: string;
test.beforeAll(async () => {
  const root = resolve(import.meta.dirname, "../..");
  server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "sidebar-single-activation-fixture",
        configureServer(vite) {
          vite.middlewares.use("/check", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module" src="/tests/sidebar/sidebar-single-activation-fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  await server.listen();
  url = `${fixtureOrigin(server)}/check`;
});
test.afterAll(async () => server?.close());

async function open(page: Page) {
  await page.goto(url);
  await expect(page.locator('[data-chat="chat-0"]')).toBeVisible();
}

test("touch sidebar keeps separate stable targets and opens on the first tap", async ({
  browser,
}) => {
  for (const width of [390, 1024] as const) {
    const context = await browser.newContext({
      hasTouch: true,
      isMobile: true,
      viewport: { width, height: 840 },
    });
    try {
      const page = await context.newPage();
      await open(page);
      const first = page.locator('[data-chat="chat-0"]');
      const row = page.locator('[data-sidebar-item="chat-0"]');
      const menu = row.locator(".row-actions");
      const before = await first.locator(".row-copy").boundingBox();
      const menuOpacity = await menu.evaluate(
        (node) => getComputedStyle(node).opacity,
      );
      await page.screenshot({
        path: test.info().outputPath(`touch-before-${width}.png`),
      });
      await first.tap();
      await expect(page.getByLabel("Opened chats")).toHaveText('["chat-0"]');
      await expect(first).toHaveAttribute("aria-current", "true");
      await expect(row).toHaveClass(/selected/);
      expect(await first.locator(".row-copy").boundingBox()).toEqual(before);
      expect(menuOpacity).toBe("1");
      await page.locator('[data-chat="chat-1"]').tap();
      await expect(page.getByLabel("Opened chats")).toHaveText(
        '["chat-0","chat-1"]',
      );
      await menu.tap();
      await expect(page.getByRole("menu")).toBeVisible();
      await expect(page.getByLabel("Opened chats")).toHaveText(
        '["chat-0","chat-1"]',
      );
      await page.screenshot({
        path: test.info().outputPath(`touch-menu-${width}.png`),
      });
    } finally {
      await context.close();
    }
  }
});

test("touch scrolling the sidebar does not activate a chat", async ({
  browser,
  browserName,
}) => {
  test.skip(
    browserName !== "chromium",
    "Trusted touch swipes use Chromium's input protocol.",
  );
  const context = await browser.newContext({
    hasTouch: true,
    isMobile: true,
    viewport: { width: 390, height: 840 },
  });
  try {
    const page = await context.newPage();
    await open(page);
    const scroll = page.locator(".chat-scroll");
    const bounds = await scroll.boundingBox();
    assert.ok(bounds);
    const input = await context.newCDPSession(page);
    const x = bounds.x + bounds.width / 2;
    const y = bounds.y + bounds.height - 60;
    await input.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ x, y }],
    });
    for (const distance of [40, 100, 180, 260] as const)
      await input.send("Input.dispatchTouchEvent", {
        type: "touchMove",
        touchPoints: [{ x, y: y - distance }],
      });
    await input.send("Input.dispatchTouchEvent", {
      type: "touchEnd",
      touchPoints: [],
    });
    await expect
      .poll(() => scroll.evaluate((node) => node.scrollTop))
      .toBeGreaterThan(0);
    await expect(page.getByLabel("Opened chats")).toHaveText("[]");
  } finally {
    await context.close();
  }
});

test("mouse and keyboard each open once and the row menu stays distinct", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1280, height: 840 });
  await open(page);
  const first = page.locator('[data-chat="chat-0"]');
  await first.click();
  await expect(page.getByLabel("Opened chats")).toHaveText('["chat-0"]');
  const second = page.locator('[data-chat="chat-1"]');
  await second.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByLabel("Opened chats")).toHaveText(
    '["chat-0","chat-1"]',
  );
  const third = page.locator('[data-chat="chat-2"]');
  await third.focus();
  await page.keyboard.press("Space");
  await expect(page.getByLabel("Opened chats")).toHaveText(
    '["chat-0","chat-1","chat-2"]',
  );
  await first.hover();
  await page
    .getByRole("button", {
      name: "Actions for Chat 0 with a long title reaching the actions",
      exact: true,
    })
    .click();
  await expect(page.getByRole("menu")).toBeVisible();
  await expect(page.getByLabel("Opened chats")).toHaveText(
    '["chat-0","chat-1","chat-2"]',
  );
});

test("desktop drag reorders without opening a chat and wheel scrolling stays native", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1280, height: 840 });
  const writes: Extract<PostBody<"/api/projects">, { action: "reorder" }>[] =
    [];
  await page.route("**/api/projects", async (route) => {
    const body = fixturePostBody<"/api/projects">(
      route.request().postDataJSON(),
    );
    assert.ok(body.action === "reorder");
    writes.push(body);
    await route.fulfill({
      json: { revision: 1, groups: body.groups },
      headers: apiSchemaHeaders(),
    });
  });
  await open(page);
  const first = page.locator('[data-chat="chat-0"]');
  await first.dragTo(page.locator('[data-chat="chat-1"]'), {
    targetPosition: { x: 24, y: 28 },
  });
  await expect.poll(() => writes.length).toBe(1);
  expect(writes[0]?.action).toBe("reorder");
  expect(
    writes[0]?.groups[JSON.stringify(["items", "/project", null])]?.slice(0, 2),
  ).toEqual(["chat-1", "chat-0"]);
  await expect(page.getByLabel("Opened chats")).toHaveText("[]");
  const scroll = page.locator(".chat-scroll");
  await scroll.hover();
  await page.mouse.wheel(0, 500);
  await expect
    .poll(() => scroll.evaluate((node) => node.scrollTop))
    .toBeGreaterThan(0);
  await expect(page.getByLabel("Opened chats")).toHaveText("[]");
});

for (const mode of ["direct", "combined"] as const) {
  test(`sidebar project groups follow the current view without losing discovery (${mode})`, async ({
    page,
  }) => {
    await page.goto(`${url}?groups=${mode}`);
    const group = (path: string) =>
      page.locator(".sidebar-project").filter({
        has: page.locator(`.project-tree-toggle[title="${path}"]`),
      });
    const filter = page.getByRole("searchbox", {
      name: "Filter projects and chats",
    });
    const switchView = async (label: string) => {
      await page.getByRole("button", { name: "Project list options" }).click();
      await page.getByRole("menuitem", { name: label, exact: true }).click();
    };
    await expect(group("/empty")).toBeVisible();
    await expect(group("/registered-archive")).toBeVisible();
    await expect(group("/active")).toBeVisible();
    await expect(group("/mixed")).toBeVisible();
    await expect(group("/shared-only")).toBeVisible();
    await expect(page.getByText("Shared room", { exact: true })).toBeVisible();
    await expect(group("/compact")).toBeVisible();
    await expect(page.getByText("compact-old-0", { exact: true })).toBeHidden();
    for (let directory = 0; directory < 5; directory++)
      await expect(group(`/archive-only-${directory}`)).toHaveCount(0);

    await group("/empty").hover();
    await group("/empty")
      .getByRole("button", { name: "Options for project Registered empty" })
      .click();
    await expect(
      page.getByRole("menuitem", { name: "Delete project…", exact: true }),
    ).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toHaveCount(0);
    await group("/active").hover();
    await group("/active")
      .getByRole("button", { name: /Options for project/ })
      .click();
    await expect(page.getByRole("menu")).toBeVisible();
    await expect(
      page.getByRole("menuitem", { name: "Delete project…", exact: true }),
    ).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toHaveCount(0);

    await filter.fill("archive-only");
    await expect(page.locator(".sidebar-project")).toHaveCount(0);
    await filter.fill("compact-old-0");
    await expect(group("/compact")).toBeVisible();
    await expect(
      page.getByText("compact-old-0", { exact: true }),
    ).toBeVisible();
    await filter.fill("");
    await switchView("Show archived chats");
    for (let directory = 0; directory < 5; directory++) {
      await expect(group(`/archive-only-${directory}`)).toBeVisible();
    }
    await expect(group("/active")).toHaveCount(0);
    await expect(group("/shared-only")).toHaveCount(0);
    await expect(group("/compact")).toHaveCount(0);
    await expect(group("/mixed")).toBeVisible();
    await expect(
      page.getByText("mixed-archived", { exact: true }),
    ).toBeVisible();
    await expect(page.getByText("mixed-active", { exact: true })).toBeHidden();
    await expect(group("/empty")).toBeVisible();
    await filter.fill("archived-3-1");
    await expect(page.locator(".sidebar-project")).toHaveCount(1);
    await expect(page.getByText("archived-3-1", { exact: true })).toBeVisible();
    await filter.fill("");
    await switchView("Show active chats");
    await expect(group("/archive-only-3")).toHaveCount(0);
    await expect(group("/active")).toBeVisible();
    await expect(group("/shared-only")).toBeVisible();
  });
}
