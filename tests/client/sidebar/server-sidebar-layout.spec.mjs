import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

test("paired sidebar keeps its controls aligned and full chat names visible", async ({
  page,
  context,
}, testInfo) => {
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  const name = "Исправить ISSUES.md Attar и слить ветки в main";
  local.snapshot.runtime.projects.push(
    ...Array.from({ length: 30 }, (_, index) => ({
      id: `/project/${index}`,
      path: `/project/${index}`,
      name: `Project ${index}`,
      folders: [],
      peerTeams: [],
    })),
  );
  Object.assign(remote.snapshot.threads[0], {
    name,
    updated: Date.now() / 1000,
  });
  local.discoverPeer(remote);
  try {
    await context.addInitScript(
      ({ source, destination }) => {
        const native = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (url.origin !== source) return native(request);
          const bytes = await request.clone().arrayBuffer();
          return native(destination + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      { source: remote.invitation.origin, destination: remote.origin },
    );
    await page.goto(local.origin);
    const sidebar = page.getByRole("complementary", {
      name: "Servers and projects",
    });
    const chat = sidebar.getByRole("button", { name: new RegExp(name) });
    await expect(chat).toBeVisible({ timeout: 20000 });
    const search = sidebar.getByRole("button", {
      name: "Search all messages",
      exact: true,
    });
    await expect(search.locator(".mantine-Button-inner")).toHaveCSS(
      "justify-content",
      "flex-start",
    );
    await expect(search.locator("svg")).toBeVisible();
    await expect(sidebar.getByLabel("Find projects and chats")).toHaveAttribute(
      "placeholder",
      "Find projects and chats",
    );
    for (const viewport of [
      { width: 1280, height: 900 },
      { width: 390, height: 844 },
    ]) {
      await page.setViewportSize(viewport);
      if (viewport.width < 761)
        await page.getByRole("button", { name: "Servers and chats" }).click();
      await page.evaluate(() => {
        document.documentElement.style.setProperty(
          "--studio-sidebar-font-size",
          "22px",
        );
      });
      await expect(chat.locator(".row-copy strong")).toHaveCSS(
        "font-size",
        "22px",
      );
      await chat.scrollIntoViewIfNeeded();
      expect(
        await chat
          .locator(".row-copy strong")
          .evaluate((element) => element.scrollWidth <= element.clientWidth),
      ).toBe(true);
      await expect(
        sidebar.getByRole("button", { name: "Studio settings" }),
      ).toBeVisible();
      await sidebar.screenshot({
        path: testInfo.outputPath(`sidebar-${viewport.width}.png`),
      });
      const projects = sidebar.locator(".server-sidebar-projects");
      await projects.evaluate((element) => {
        element.scrollTop = element.scrollHeight;
      });
      await expect(search).toBeInViewport();
      await expect(
        sidebar.getByRole("button", { name: "Studio settings" }),
      ).toBeInViewport();
      await projects.evaluate((element) => {
        element.scrollTop = 0;
      });
    }
    await sidebar.getByRole("button", { name: "Studio settings" }).click();
    await expect(
      page.getByRole("dialog", { name: "Studio settings" }),
    ).toBeVisible();
  } finally {
    await Promise.all([local.close(), remote.close()]);
  }
});
