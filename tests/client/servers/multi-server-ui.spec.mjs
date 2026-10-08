import {
  test,
  expect,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
} from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";
import { execFileSync } from "node:child_process";
test("one UI routes overlapping chats to two and three signed servers without reload", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(180000);
  page.setDefaultTimeout(15000);
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  const third = await fixture("Third", true);
  const errors = [];
  let draftQuotaFault = false;
  page.on("pageerror", (error) => {
    // The storage adapter also reports the deliberate quota fault globally.
    if (draftQuotaFault && error.name === "QuotaExceededError") return;
    errors.push(error.message);
  });
  try {
    await context.addInitScript(
      ({ destinations }) => {
        window.__studioNotifications = [];
        window.Notification = class {
          static permission = "granted";
          constructor(title, options) {
            this.title = title;
            this.options = options;
            window.__studioNotifications.push(this);
          }
          close() {}
        };
        const nativeFetch = window.fetch.bind(window);
        // A test-only network shim keeps HTTPS identities while the fixture uses
        // isolated loopback ports. All signatures are verified by the HTTP fixture.
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (!destinations[url.origin]) return nativeFetch(request);
          const bytes = await request.clone().arrayBuffer();
          const response = await nativeFetch(
            destinations[url.origin] + url.pathname + url.search,
            {
              method: request.method,
              headers: request.headers,
              ...(bytes.byteLength ? { body: bytes } : {}),
              signal: request.signal,
              redirect: "error",
              credentials: "omit",
            },
          );
          if (
            url.pathname.endsWith("/pair") &&
            url.hostname.startsWith("remote.") &&
            !sessionStorage.getItem("studio-test-pair-response-lost")
          ) {
            sessionStorage.setItem("studio-test-pair-response-lost", "1");
            await response.text();
            throw new TypeError("Pair response lost in fixture");
          }
          return response;
        };
      },
      {
        destinations: {
          [remote.invitation.origin]: remote.origin,
          [third.invitation.origin]: third.origin,
        },
      },
    );
    await page.goto(local.origin);
    await page.locator("#message").waitFor();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Server access", exact: true }).click();
    const access = page.getByRole("region", {
      name: "Server access",
      exact: true,
    });
    await access
      .getByRole("button", { name: "Create pairing invitation", exact: true })
      .click();
    await expect(access.getByLabel("Pairing invitation")).toHaveValue(
      JSON.stringify(local.invitation, null, 2),
    );
    await access
      .getByRole("button", { name: "Revoke access", exact: true })
      .click();
    await expect(
      access.getByRole("button", { name: "Revoke access", exact: true }),
    ).toBeDisabled();
    await page
      .getByRole("dialog")
      .filter({
        has: page.getByRole("heading", {
          name: "Studio settings",
          exact: true,
        }),
      })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    const pair = async (server) => {
      const dialog = page.getByRole("dialog", { name: "Studio servers" });
      await expect(dialog).toBeVisible();
      await dialog.getByLabel("Server address").fill(server.invitation.origin);
      await dialog
        .getByLabel("Pairing invitation")
        .fill(JSON.stringify(server.invitation));
      await dialog
        .getByRole("button", { name: "Pair server", exact: true })
        .click();
      await expect(
        page.locator(
          `[data-server="${server.invitation.serverId}"] .server-chat`,
        ),
      ).toContainText(`${server.invitation.label} chat`);
      await dialog.getByRole("button", { name: "Close", exact: true }).click();
    };
    await page.getByRole("button", { name: "Servers", exact: true }).click();
    const firstPair = page.getByRole("dialog", { name: "Studio servers" });
    await expect(firstPair).toBeVisible();
    await firstPair.getByLabel("Server address").fill(remote.invitation.origin);
    await firstPair
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await firstPair
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    await expect(firstPair.getByRole("alert")).toHaveText(
      "Pair response lost in fixture",
    );
    await page.reload();
    await page.getByRole("button", { name: "Servers", exact: true }).click();
    await pair(remote);
    expect(remote.pairs).toHaveLength(2);
    expect(remote.pairs[0]).toBe(remote.pairs[1]);
    const cdp = await context.newCDPSession(page);
    await cdp.send("Performance.enable");
    const browserCdp = await context.browser().newBrowserCDPSession();
    const measurements = [];
    const measure = async (servers, stage, startupMs) => {
      let heap = (await cdp.send("Performance.getMetrics")).metrics.find(
        (row) => row.name === "JSHeapUsedSize",
      ).value;
      for (const frame of page
        .frames()
        .filter((frame) => frame !== page.mainFrame())) {
        let frameCdp;
        try {
          frameCdp = await context.newCDPSession(frame);
        } catch (error) {
          if (error.message.includes("part of the parent frame's session"))
            continue;
          throw error;
        }
        await frameCdp.send("Performance.enable");
        heap += (await frameCdp.send("Performance.getMetrics")).metrics.find(
          (row) => row.name === "JSHeapUsedSize",
        ).value;
        await frameCdp.detach();
      }
      const processes = await browserCdp.send("SystemInfo.getProcessInfo");
      const ids = processes.processInfo
        .map((row) => row.id)
        .filter((id) => Number.isInteger(id) && id > 0);
      const rss =
        execFileSync("ps", ["-o", "rss=", "-p", ids.join(",")], {
          encoding: "utf8",
        })
          .trim()
          .split(/\s+/)
          .reduce((sum, value) => sum + Number(value), 0) * 1024;
      return {
        servers,
        stage,
        startupMs,
        jsHeapUsedBytes: heap,
        browserResidentBytes: rss,
        frames: page.frames().length,
      };
    };
    await page.clock.install();
    for (const count of [2, 3]) {
      if (count === 3) {
        await page.getByRole("button", { name: "Manage", exact: true }).click();
        await pair(third);
      }
      const start = Date.now();
      await page.reload();
      await expect(
        page.locator(".server-sidebar [data-chat='overlap']"),
      ).toHaveCount(count);
      const startupMs = Date.now() - start;
      await page.locator('[data-server="local"] .server-chat').click();
      await page
        .frameLocator('iframe[title="Studio on This computer"]')
        .locator("#message")
        .fill(`idle draft ${count}`);
      await page
        .locator(
          `[data-server="${count === 2 ? "remote" : "third"}"] .server-chat`,
        )
        .click();
      measurements.push(await measure(count, "mounted", startupMs));
      await page.clock.fastForward(5 * 60 * 1000 + 30000);
      await expect(page.locator("iframe")).toHaveCount(1);
      const afterIdle = await measure(count, "idle-unloaded", null);
      measurements.push(afterIdle);
      local.complete(`idle-${count}`);
      await page.clock.fastForward(30000);
      await expect(
        page.locator('[data-server="local"] .server-chat'),
      ).toContainText("●");
      await expect
        .poll(() =>
          page.evaluate(() =>
            window.__studioNotifications.some((row) =>
              row.title.includes("Local chat: Reply ready"),
            ),
          ),
        )
        .toBe(true);
      // Remount each view before the existing caller checks below.
      for (const id of count === 2
        ? ["local", "remote"]
        : ["local", "remote", "third"]) {
        await page.locator(`[data-server="${id}"] .server-chat`).click();
        await expect(
          page
            .locator(`iframe[src*="studio-server=${id}"]`)
            .contentFrame()
            .locator("#message"),
        ).toBeVisible();
        if (id === "local")
          await expect(
            page
              .locator(`iframe[src*="studio-server=${id}"]`)
              .contentFrame()
              .locator("#message"),
          ).toHaveValue(`idle draft ${count}`);
      }
    }
    await page.locator('[data-server="local"] .server-chat').click();
    const localFrame = page.frameLocator(
      'iframe[title="Studio on This computer"]',
    );
    const remoteFrame = page.frameLocator('iframe[title="Studio on Remote"]');
    const timeOrigins = await Promise.all(
      page
        .frames()
        .filter((frame) => frame !== page.mainFrame())
        .map((frame) => frame.evaluate(() => performance.timeOrigin)),
    );
    await localFrame.locator("#message").fill("local draft");
    await page.locator('[data-server="remote"] .server-chat').click();
    await remoteFrame.locator("#message").fill("remote draft");
    await page.locator('[data-server="local"] .server-chat').click();
    await expect(localFrame.locator("#message")).toHaveValue("local draft");
    await page.locator('[data-server="remote"] .server-chat').click();
    await expect(remoteFrame.locator("#message")).toHaveValue("remote draft");
    expect(
      await Promise.all(
        page
          .frames()
          .filter((frame) => frame !== page.mainFrame())
          .map((frame) => frame.evaluate(() => performance.timeOrigin)),
      ),
    ).toEqual(timeOrigins);
    await remoteFrame
      .getByRole("button", { name: "Send message", exact: true })
      .click();
    await expect.poll(() => remote.writes.length).toBe(1);
    expect(local.writes).toHaveLength(0);
    expect(remote.writes[0].text).toBe("remote draft");
    await page
      .getByRole("button", { name: "Search all messages", exact: true })
      .click();
    const search = page.getByRole("dialog", { name: "Search all servers" });
    await search
      .getByLabel("Search all messages", { exact: true })
      .fill("shared term");
    await search.getByRole("button", { name: "Search", exact: true }).click();
    await expect(search.locator(".server-search-result")).toHaveCount(3);
    await search.getByRole("button", { name: "Close", exact: true }).click();
    remote.offline(true);
    await expect(
      page.locator('[data-server="remote"] [role="status"]'),
    ).toHaveText("Offline");
    await expect(remoteFrame.locator("#message")).toBeVisible();
    remote.offline(false);
    await expect(
      page.locator('[data-server="remote"] [role="status"]'),
    ).toHaveText("Online", { timeout: 20000 });
    await page.evaluate(() => {
      window.__studioNotifications = [];
    });
    local.complete();
    third.complete();
    await expect(
      page.getByLabel("2 unread chats", { exact: true }),
    ).toBeVisible();
    await expect
      .poll(() => page.evaluate(() => window.__studioNotifications.length))
      .toBe(2);
    expect(
      await page.evaluate(() =>
        window.__studioNotifications.map((notice) => notice.title).sort(),
      ),
    ).toEqual([
      "Third: Third chat: Reply ready",
      "This computer: Local chat: Reply ready",
    ]);
    await remoteFrame.locator("#message").press("Control+k");
    await expect(
      page.getByRole("dialog", { name: "Search all servers" }),
    ).toBeVisible();
    await page
      .getByRole("dialog", { name: "Search all servers" })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await page
      .locator('[data-server="remote"] .server-tools')
      .getByRole("button", { name: "Settings", exact: true })
      .click();
    await remoteFrame
      .getByRole("tab", { name: "Appearance", exact: true })
      .click();
    await remoteFrame
      .getByLabel("Studio theme", { exact: true })
      .selectOption("dark");
    await remoteFrame
      .getByRole("dialog")
      .filter({
        has: remoteFrame.getByRole("heading", {
          name: "Studio settings",
          exact: true,
        }),
      })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await expect(page.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await expect(localFrame.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await expect(remoteFrame.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await page.setViewportSize({ width: 600, height: 800 });
    await page
      .getByRole("button", { name: "Servers and chats", exact: true })
      .click();
    await expect(
      page.getByRole("complementary", { name: "Servers and projects" }),
    ).toBeVisible();
    await page.locator('[data-server="remote"] .server-chat').click();
    await expect(
      page.getByRole("complementary", { name: "Servers and projects" }),
    ).not.toBeVisible();
    await remoteFrame
      .getByRole("button", { name: "Toggle conversations", exact: true })
      .click();
    await expect(
      page.getByRole("complementary", { name: "Servers and projects" }),
    ).toBeVisible();
    await page.setViewportSize({ width: 1280, height: 720 });
    const remoteWindow = page
      .frames()
      .find(
        (frame) =>
          new URL(frame.url()).searchParams.get("studio-server") === "remote",
      );
    const frameSecurity = await remoteWindow.evaluate(async () => {
      let parentBlocked = false,
        siblingBlocked = false;
      try {
        void parent.document;
      } catch (error) {
        parentBlocked = error.name === "SecurityError";
      }
      try {
        void parent.frames[0].document;
      } catch (error) {
        siblingBlocked = error.name === "SecurityError";
      }
      const correlation = crypto.randomUUID();
      const blocked = await new Promise((resolve) => {
        const receive = (event) => {
          if (
            event.data?.kind === "studio-server-transport-result" &&
            event.data.correlation === correlation
          ) {
            removeEventListener("message", receive);
            resolve(event.data.error);
          }
        };
        addEventListener("message", receive);
        parent.postMessage(
          {
            kind: "studio-server-transport",
            correlation,
            value: {
              action: "request",
              serverId: "third",
              streamId: "attack",
              url: "https://third.tailnet.ts.net/api/session",
              method: "GET",
            },
          },
          new URLSearchParams(location.search).get("studio-parent"),
        );
      });
      return { parentBlocked, siblingBlocked, blocked };
    });
    expect(frameSecurity.parentBlocked).toBe(true);
    expect(frameSecurity.siblingBlocked).toBe(true);
    expect(frameSecurity.blocked).toContain("Invalid server frame owner");
    const exportHeaders = await remoteWindow.evaluate(async () => {
      const correlation = crypto.randomUUID();
      return new Promise((resolve) => {
        const receive = (event) => {
          if (
            event.data?.kind === "studio-server-transport-result" &&
            event.data.correlation === correlation
          ) {
            removeEventListener("message", receive);
            resolve(event.data.result?.headers);
          }
        };
        addEventListener("message", receive);
        parent.postMessage(
          {
            kind: "studio-server-transport",
            correlation,
            value: {
              action: "request",
              serverId: "remote",
              streamId: "monitor-export",
              url: "https://remote.tailnet.ts.net/api/monitor/log?id=monitor",
              method: "GET",
            },
          },
          new URLSearchParams(location.search).get("studio-parent"),
        );
      });
    });
    const exported = new Headers(exportHeaders);
    expect(exported.get("Content-Disposition")).toBe(
      'attachment; filename="remote-monitor.log"',
    );
    expect(exported.get("X-Log-Truncated")).toBe("true");
    draftQuotaFault = true;
    await remoteWindow.evaluate(() => {
      const original = Storage.prototype.setItem;
      const put = IDBObjectStore.prototype.put,
        add = IDBObjectStore.prototype.add;
      window.__restoreDraftStorage = () => {
        Storage.prototype.setItem = original;
        IDBObjectStore.prototype.put = put;
        IDBObjectStore.prototype.add = add;
      };
      for (const method of ["put", "add"]) {
        const originalMethod = IDBObjectStore.prototype[method];
        IDBObjectStore.prototype[method] = function (...args) {
          if (this.transaction.db.name.endsWith("--drafts"))
            throw new DOMException(
              "Full fixture storage",
              "QuotaExceededError",
            );
          return originalMethod.apply(this, args);
        };
      }
      Storage.prototype.setItem = function (key, value) {
        if (
          key.includes("codex-chat-draft:") ||
          (key.includes("codex-drafts:") && key.includes(":pending:"))
        )
          throw new DOMException("Full fixture storage", "QuotaExceededError");
        original.call(this, key, value);
      };
    });
    await remoteFrame.locator("#message").fill("unsaved draft must stay open");
    await expect(remoteFrame.locator("[data-draft-sync-status]")).toContainText(
      "Keep this chat open",
    );
    await page.locator('[data-server="third"] .server-chat').click();
    await page.clock.fastForward(5 * 60 * 1000 + 30000);
    await expect(page.locator('iframe[title="Studio on Remote"]')).toHaveCount(
      1,
    );
    await page.locator('[data-server="remote"] .server-chat').click();
    await expect(remoteFrame.locator("#message")).toHaveValue(
      "unsaved draft must stay open",
    );
    await remoteWindow.evaluate(() => window.__restoreDraftStorage());
    draftQuotaFault = false;
    await remoteWindow.evaluate(async () => {
      const registration =
        await navigator.serviceWorker.register("/studio-sw.js");
      await navigator.serviceWorker.ready;
      if (registration.waiting)
        registration.waiting.postMessage({ type: "STUDIO_SKIP_WAITING" });
    });
    await context.setOffline(true);
    await remoteWindow.goto(remoteWindow.url());
    await expect(remoteFrame.locator("#message")).toBeVisible();
    await context.setOffline(false);
    await page.screenshot({
      path: testInfo.outputPath("multi-server-ui.png"),
      fullPage: true,
    });
    expect(errors).toEqual([]);
    await testInfo.attach("multi-server-measurements.json", {
      body: JSON.stringify(measurements),
      contentType: "application/json",
    });
    console.log(JSON.stringify({ multiServerMeasurements: measurements }));
  } finally {
    await page.goto("about:blank").catch(() => {});
    await Promise.all([local.close(), remote.close(), third.close()]);
  }
});

test("phone HTTPS Serve route opens the normal App and its local session", async ({
  page,
  context,
}) => {
  const local = await fixture("Phone");
  try {
    await context.route("https://phone.tailnet.ts.net/**", async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === "/api/sync/stream") {
        await route.fulfill({
          status: 200,
          contentType: "text/event-stream",
          headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
          body: apiSchemaHandshakeSse("event: resources\ndata: []\n\n"),
        });
        return;
      }
      const response = await route.fetch({
        url: local.origin + url.pathname + url.search,
      });
      await route.fulfill({ response });
    });
    await page.goto("https://phone.tailnet.ts.net/");
    await expect(page.locator("#message")).toBeVisible();
    await expect(
      page.getByRole("dialog", { name: "Studio servers" }),
    ).toHaveCount(0);
    await expect(page.locator("iframe")).toHaveCount(0);
    const sent = await page.evaluate(async () => {
      const session = await (await fetch("/api/session")).json();
      const response = await fetch("/api/messages", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": session.token,
        },
        body: JSON.stringify({ id: crypto.randomUUID(), text: "phone send" }),
      });
      return response.status;
    });
    expect(sent).toBe(200);
    expect(local.writes[0]._canvasToken).toBe("local-only");
  } finally {
    await page.goto("about:blank").catch(() => {});
    await local.close();
  }
});
