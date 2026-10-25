import { test, expect } from "../playwright.mjs";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.mjs";

test.use({ hasTouch: true });
for (const check of [
  "buttons",
  "header",
  "title",
  "background",
  "panel-background",
]) {
  test(`Duo rail mode ${check} preserves the pane and normal headers`, async ({
    page,
    context,
  }) => {
    const errors = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" && msg.text().includes("Studio display error"))
        errors.push(msg.text());
    });
    const fixture = await sidebarParityFixture(context, { paired: false });
    const pending = structuredClone(fixture.local.snapshot.runtime.requests);
    fixture.local.snapshot.runtime.requests = [];
    fixture.local.snapshot.runtime.complaints = [];
    const cdp = await context.newCDPSession(page);
    try {
      await page.setViewportSize({ width: 1200, height: 669 });
      const sidebar = await fixture.open(page);
      await sidebar
        .getByRole("button", { name: /^Local Saved pin\b/ })
        .first()
        .click();
      for (const width of [1200, 390]) {
        await page.setViewportSize({ width, height: 669 });
        await expect(page.locator(".workspace-header")).toBeVisible();
        await expect(
          page.locator(".workspace-header #conversation-title"),
        ).toHaveText("Local Saved pin");
        await expect(page.locator("#messages #conversation-title")).toHaveCount(
          0,
        );
      }
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 59, bottom: 34, left: 0 },
      });
      await page.setViewportSize({ width: 951, height: 669 });
      await expect(page.locator(".workspace-header")).toBeVisible();
      await expect(
        page.getByRole("navigation", { name: "Conversation actions" }),
      ).toHaveCount(0);
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 84, bottom: 34, left: 0 },
      });
      await page.setViewportSize({ width: 951, height: 669 });
      const rail = page.getByRole("navigation", {
        name: "Conversation actions",
      });
      await expect(rail).toBeVisible();
      await expect(
        page.getByText("Could not display this conversation"),
      ).toHaveCount(0);
      if (check === "buttons") {
        await expect(rail.getByRole("button")).toHaveCount(2);
        await expect(rail.getByRole("button")).toHaveText(["", ""]);
        await expect(
          rail.getByRole("button", {
            name: "Toggle conversations",
            exact: true,
          }),
        ).toBeVisible();
        await expect(
          rail.getByRole("button", { name: "Chat actions", exact: true }),
        ).toBeVisible();
        const radii = await rail.getByRole("button").evaluateAll((nodes) =>
          nodes.map((node) => ({
            radius: getComputedStyle(node).borderRadius,
            width: node.getBoundingClientRect().width,
            background: getComputedStyle(node).backgroundColor,
          })),
        );
        expect(
          radii.every((x) => parseFloat(x.radius) >= 24 && x.width === 48),
        ).toBe(true);
        expect(
          radii.every(
            (x) => x.background.includes("rgba") || x.background.includes("/"),
          ),
        ).toBe(true);
        fixture.local.snapshot.runtime.requests = pending.map((request) => ({
          ...request,
          agent: "overlap",
        }));
        fixture.local.complete("attention");
        await expect(rail.getByRole("button")).toHaveCount(3);
        await expect(
          rail.getByRole("button", { name: "Messages, 1 need you" }),
        ).toBeVisible();
        const sidebarToggle = rail.getByRole("button", {
          name: "Toggle conversations",
          exact: true,
        });
        const messagesToggle = rail.getByRole("button", {
          name: "Messages, 1 need you",
          exact: true,
        });
        const more = rail.getByRole("button", {
          name: "Chat actions",
          exact: true,
        });
        await sidebarToggle.focus();
        await page.keyboard.press("Tab");
        await expect(messagesToggle).toBeFocused();
        await page.keyboard.press("Tab");
        await expect(more).toBeFocused();
        await page.keyboard.press("Enter");
        const outline = page.getByRole("menuitem", {
          name: "Browse prompts",
          exact: true,
        });
        await expect(outline).toBeFocused();
        const menuBounds = await page.getByRole("menu").boundingBox();
        expect(menuBounds.x + menuBounds.width).toBeLessThanOrEqual(867.5);
        expect(menuBounds.y + menuBounds.height).toBeLessThanOrEqual(635.5);
        await page.keyboard.press("Enter");
        await expect(
          page.getByRole("dialog", { name: "Prompt history" }),
        ).toBeVisible();
        await expect(
          page.getByRole("textbox", { name: "Search this chat" }),
        ).toBeFocused();
        await page.keyboard.press("Escape");
        await page.keyboard.press("Escape");
        await expect(more).toBeFocused();
      } else if (check === "header") {
        await expect(page.locator(".workspace-header")).toHaveCount(0);
      } else if (check === "title") {
        const heading = page.locator(
          "#messages .message-content > .transcript-heading:first-child",
        );
        await expect(heading).toBeVisible();
        await expect(heading.locator("#conversation-title")).toHaveText(
          "Local Saved pin",
        );
        expect(await heading.locator("button,a,[tabindex]").count()).toBe(0);
        await expect(heading).toContainText("Home project");
      } else {
        if (check === "panel-background") {
          await rail
            .getByRole("button", { name: "Chat actions", exact: true })
            .click();
          await page
            .getByRole("menuitem", { name: "Changes", exact: true })
            .click();
          await expect(
            page.locator(
              ".workspace-drawer .mantine-Drawer-content, .workspace-panel-inline:not([hidden])",
            ),
          ).toBeVisible();
        }
        const png = (await page.screenshot()).toString("base64");
        const colors = await page.evaluate(async (png) => {
          const image = new Image();
          image.src = "data:image/png;base64," + png;
          await image.decode();
          const canvas = document.createElement("canvas");
          canvas.width = image.width;
          canvas.height = image.height;
          const ctx = canvas.getContext("2d");
          ctx.drawImage(image, 0, 0);
          return [
            [850, 300],
            [911, 300],
            [850, 650],
            [911, 650],
          ].map(([x, y]) => [...ctx.getImageData(x, y, 1, 1).data]);
        }, png);
        expect(colors[1]).toEqual(colors[0]);
        if (check === "panel-background") {
          expect(colors[2]).toEqual(colors[0]);
          expect(colors[3]).toEqual(colors[0]);
        }
      }
      for (const target of await page
        .locator(
          "main.workspace button, main.workspace textarea, main.workspace input, .workspace-drawer button, .workspace-drawer input, .workspace-drawer textarea",
        )
        .all()) {
        if (!(await target.isVisible())) continue;
        const r = await target.boundingBox();
        if (!r || !r.width || !r.height) continue;
        expect(r.x + r.width).toBeLessThanOrEqual(867.5);
        expect(r.y + r.height).toBeLessThanOrEqual(635.5);
      }
      await page.screenshot({
        path: test.info().outputPath(`rail-${check}.png`),
      });
      if (check === "panel-background") {
        await rail
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await expect(
          page.getByRole("menuitem", { name: "Browse prompts", exact: true }),
        ).toBeVisible();
        await page.keyboard.press("Escape");
        await page.keyboard.press("Escape");
        await expect(
          page.locator(".workspace-drawer .mantine-Drawer-content"),
        ).toBeHidden();
      }
      const escapedText = await page
        .locator("main.workspace")
        .evaluate((root) => {
          const nodes = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
          const escaped = [];
          while (nodes.nextNode()) {
            const node = nodes.currentNode;
            if (
              !node.textContent.trim() ||
              node.parentElement.closest('[aria-hidden="true"],script,style')
            )
              continue;
            const range = document.createRange();
            range.selectNodeContents(node);
            for (const rect of range.getClientRects()) {
              let left = rect.left,
                right = rect.right,
                top = rect.top,
                bottom = rect.bottom;
              for (
                let parent = node.parentElement;
                parent && parent !== root.parentElement;
                parent = parent.parentElement
              ) {
                const style = getComputedStyle(parent);
                const clip = parent.getBoundingClientRect();
                if (style.overflowX !== "visible") {
                  left = Math.max(left, clip.left);
                  right = Math.min(right, clip.right);
                }
                if (style.overflowY !== "visible") {
                  top = Math.max(top, clip.top);
                  bottom = Math.min(bottom, clip.bottom);
                }
              }
              if (
                right > left &&
                bottom > top &&
                (right > 867.5 || bottom > 635.5)
              )
                escaped.push({
                  text: node.textContent.slice(0, 40),
                  right,
                  bottom,
                });
            }
          }
          return escaped;
        });
      expect(escapedText).toEqual([]);
      await page.keyboard.press("Escape");
      await page.keyboard.press("Escape");
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 0, bottom: 0, left: 0 },
      });
      for (const width of [1200, 390]) {
        await page.setViewportSize({ width, height: 669 });
        await expect(
          page.locator(".workspace-header #conversation-title"),
        ).toHaveText("Local Saved pin");
        await expect(page.locator("#messages #conversation-title")).toHaveCount(
          0,
        );
      }
      expect(errors).toEqual([]);
    } finally {
      await fixture.close();
    }
  });
}
