import { required } from "../teams/helpers.ts";
import { commandOf, sendBody } from "../messages/send-command.ts";
import {
  FIXTURE_OPTIMIZE_DEPS,
  installApiSchemaFixtureHeaders,
  test,
  expect,
  spawnFixture,
  rustUiFixture,
} from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { ViteDevServer } from "vite";

// Real client and isolated prebuilt server; no native model requests.
for (const [device, viewport] of [
  ["desktop", { width: 1440, height: 960 }],
  ["mobile", { width: 390, height: 844 }],
] as const) {
  test(`${device}: failed message does not block another and Retry repeats its command`, async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
    const evidence = await mkdtemp(join(tmpdir(), "studio-outbox-controls-"));
    const fixture = spawnFixture(process.execPath, [rustUiFixture, evidence], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
    });
    let log = "";
    fixture.stderr!.on("data", (chunk) => {
      log += chunk;
    });
    let server: ViteDevServer | undefined;
    const context = await browser.newContext({
      viewport,
      isMobile: device === "mobile",
      serviceWorkers: "block",
    });
    try {
      const port = await new Promise<number>((resolve, reject) => {
        fixture.stdout!.once("data", (chunk) =>
          resolve(Number(String(chunk).trim())),
        );
        fixture.once("exit", () => reject(new Error(log)));
      });
      const target = `http://127.0.0.1:${port}`;
      const { createFixtureViteServer } = await import("../playwright.ts");
      server = await createFixtureViteServer({
        optimizeDeps: { ...FIXTURE_OPTIMIZE_DEPS },
        configFile: false,
        root: join(root, "workspaces/client/apps/web"),
        cacheDir: join(evidence, "vite"),
        server: {
          host: "127.0.0.1",
          port: 0,
          proxy: {
            "/api": {
              target,
              changeOrigin: true,
              configure(proxy) {
                proxy.on("proxyReq", (request) =>
                  request.setHeader("origin", target),
                );
              },
            },
          },
        },
      });
      await server.listen();
      installApiSchemaFixtureHeaders(context);
      const page = await context.newPage();
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      const commands: unknown[] = [];
      let loseFirst = true;
      await context.route("**/api/commands", async (route) => {
        const command = commandOf(route.request(), "send");
        if (!command) return route.fallback();
        commands.push(command);
        if (loseFirst) {
          loseFirst = false;
          return route.abort("failed");
        }
        return route.fallback();
      });
      await page.goto(required(required(server.resolvedUrls).local[0]));
      if (device === "mobile")
        await page.getByText("Release lead", { exact: true }).click();
      const composer = page.locator("#message");
      await composer.waitFor();
      await composer.fill("First message");
      await page.locator("#send").click();
      const first = page
        .locator(".message")
        .filter({ hasText: "First message" });
      await expect(
        first.getByRole("button", { name: "Retry", exact: true }),
      ).toBeVisible();
      await composer.fill("Second message");
      await page.locator("#send").click();
      await expect.poll(() => commands.length).toBe(2);
      expect(sendBody(commands[1] as never).text).toBe("Second message");
      await first.getByRole("button", { name: "Retry", exact: true }).click();
      await expect.poll(() => commands.length).toBe(3);
      expect(commands[2]).toEqual(commands[0]);
      // The server applies this request before its response is lost. This
      // independent outbox cannot see the page's live receipt, so Retry must
      // repeat the exact command and the server must replay its result.
      const replay = await page.evaluate(
        async (room) => {
          const { createOutbox } = await import("/src/sync/send.ts");
          const { createCommandClient } = await import("/src/commands.ts");
          const { randomUUID } = await import("/src/uuid.ts");
          const identity = await (await fetch("/api/sync/identity")).json();
          const session = await (await fetch("/api/session")).json();
          let drop = true;
          const requests: unknown[] = [];
          const command = createCommandClient(sessionStorage, async (body) => {
            requests.push(body);
            const response = await fetch("/api/commands", {
              method: "POST",
              headers: {
                "Content-Type": "application/json",
                "X-Canvas-Token": session.token,
                "X-Canvas-Workspace": identity.workspaceId,
              },
              body: JSON.stringify(body),
            });
            const result = await response.json();
            if (!response.ok)
              throw new Error(`Command failed: ${response.status}`);
            if (drop) {
              drop = false;
              throw new TypeError("Lost response");
            }
            return result;
          });
          const outbox = createOutbox({
            api: {
              command,
              getStreamSession: () => ({ workspaceId: identity.workspaceId }),
            } as never,
            storage: sessionStorage,
            watchResourceConnection: () => () => {},
          });
          const body = {
            id: randomUUID(),
            room,
            text: "Applied before response loss",
          };
          const first = await outbox.durableSend(body);
          const second = await outbox.changeOutbox(body.id, "resume");
          return { requests, first, second };
        },
        sendBody(commands[0] as never).room,
      );
      expect(replay.first.status).toBe("uncertain");
      expect(replay.second?.kind).toBe("server");
      expect(replay.requests).toHaveLength(2);
      expect(replay.requests[1]).toEqual(replay.requests[0]);
      expect(errors).toEqual([]);
    } finally {
      await context.close();
      await server?.close();
      fixture.kill("SIGTERM");
      await rm(evidence, { recursive: true, force: true });
      await rm(`${evidence}-fixture-state`, { recursive: true, force: true });
    }
  });
}
