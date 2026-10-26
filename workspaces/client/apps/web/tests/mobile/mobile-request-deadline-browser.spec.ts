import {
  chromium,
  webkit,
  type BrowserType,
  type Route,
} from "playwright-core";
import { fixtureServerOrigin } from "./browser-fixture.ts";
declare global {
  interface Window {
    mobileDeadline: {
      outcomes: Record<string, string>;
      cancelWithDeadline: AbortController;
      cancelOnly: AbortController;
    };
  }
}
import { FIXTURE_OPTIMIZE_DEPS } from "../playwright.ts";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import {
  test,
  expect,
  browserExecutablePath,
  skipIfWebKitCannotLaunch,
} from "../playwright.ts";

const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));

const webkitPath = webkit.executablePath();

async function runDeadlineChecks(
  name: string,
  engine: BrowserType,
  executablePath: string,
) {
  // Browser request deadlines still expire after mobile JavaScript suspension.
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    optimizeDeps: { ...FIXTURE_OPTIMIZE_DEPS },
    configFile: false,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    let browser;
    try {
      browser = await engine.launch({ headless: true, executablePath });
    } catch (error) {
      if (name === "webkit") skipIfWebKitCannotLaunch(test, error);
      throw error;
    }
    try {
      const page = await browser.newPage();
      await page.route("**/deadline-fixture", (r) =>
        r.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Deadline fixture</title>",
        }),
      );
      const held = new Map<string, Route>();
      await page.route("**/api/held-*", (r) => {
        held.set(new URL(r.request().url()).pathname, r);
      });
      await page.goto(fixtureServerOrigin(server) + "deadline-fixture");
      await page.clock.install();
      const { start, readDeadline } = await page.evaluate(async () => {
        const { get, post, syncPost, READ_DEADLINE_MS } =
          await import("/src/api.ts");
        window.mobileDeadline = {
          outcomes: {},
          cancelWithDeadline: new AbortController(),
          cancelOnly: new AbortController(),
        };
        const record = (key: string, p: Promise<unknown>) =>
          p.then(
            () => (window.mobileDeadline.outcomes[key] = "success"),
            (e) => (window.mobileDeadline.outcomes[key] = e.name),
          );
        record(
          "read",
          get(
            // @ts-expect-error Synthetic held endpoint tests generic transport deadlines.
            "/api/held-read",
          ),
        );
        record(
          "durable",
          syncPost(
            // @ts-expect-error Synthetic held endpoint tests generic transport deadlines.
            "/api/held-durable",
            { id: "fixed-id" },
          ),
        );
        record(
          "ordinary",
          post(
            // @ts-expect-error Synthetic held endpoint tests generic transport deadlines.
            "/api/held-ordinary",
            { action: "one-time" },
          ),
        );
        return { start: Date.now(), readDeadline: READ_DEADLINE_MS };
      });
      await expect.poll(() => held.size).toBe(3);
      await page.clock.setSystemTime(start + 16_000);
      await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
      await page.waitForFunction(() => window.mobileDeadline.outcomes.durable);
      assert.deepEqual(
        await page.evaluate(() => window.mobileDeadline.outcomes),
        {
          durable: "NetworkTimeoutError",
        },
      );
      await page.clock.setSystemTime(start + readDeadline + 1);
      await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
      await page.waitForFunction(() => window.mobileDeadline.outcomes.read);
      assert.deepEqual(
        await page.evaluate(() => window.mobileDeadline.outcomes),
        {
          read: "NetworkTimeoutError",
          durable: "NetworkTimeoutError",
        },
      );
      const ordinary = held.get("/api/held-ordinary");
      assert.ok(ordinary);
      await ordinary.fulfill({ json: { ok: true } });
      await page.waitForFunction(
        () => window.mobileDeadline.outcomes.ordinary === "success",
      );
      await page.evaluate(async () => {
        const { get, post } = await import("/src/api.ts");
        const record = (key: string, promise: Promise<unknown>) =>
          promise.then(
            () => (window.mobileDeadline.outcomes[key] = "success"),
            (error) => (window.mobileDeadline.outcomes[key] = error.name),
          );
        window.mobileDeadline.cancelWithDeadline = new AbortController();
        window.mobileDeadline.cancelOnly = new AbortController();
        const alreadyAborted = new AbortController();
        alreadyAborted.abort();
        record(
          "cancelledRead",
          get(
            // @ts-expect-error Synthetic held endpoint tests cancellation.
            "/api/held-cancel-read",
            {
              signal: window.mobileDeadline.cancelWithDeadline.signal,
              timeoutMs: 15000,
            },
          ),
        );
        record(
          "cancelledPost",
          post(
            // @ts-expect-error Synthetic held endpoint tests cancellation.
            "/api/held-cancel-post",
            {},
            {
              signal: window.mobileDeadline.cancelOnly.signal,
            },
          ),
        );
        record(
          "alreadyAborted",
          post(
            // @ts-expect-error Synthetic held endpoint tests cancellation before transport.
            "/api/held-already-aborted",
            {},
            {
              signal: alreadyAborted.signal,
              timeoutMs: 15000,
            },
          ),
        );
      });
      await expect
        .poll(
          () =>
            held.has("/api/held-cancel-read") &&
            held.has("/api/held-cancel-post"),
        )
        .toBe(true);
      await page.evaluate(() => {
        window.mobileDeadline.cancelWithDeadline.abort();
        window.mobileDeadline.cancelOnly.abort();
      });
      await page.waitForFunction(
        () =>
          window.mobileDeadline.outcomes.cancelledRead &&
          window.mobileDeadline.outcomes.cancelledPost,
      );
      assert.deepEqual(
        await page.evaluate(() => ({
          read: window.mobileDeadline.outcomes.cancelledRead,
          post: window.mobileDeadline.outcomes.cancelledPost,
          beforeStart: window.mobileDeadline.outcomes.alreadyAborted,
        })),
        { read: "AbortError", post: "AbortError", beforeStart: "AbortError" },
      );
      assert(!held.has("/api/held-already-aborted"));
      console.log(
        `${name}: deadlines release on resume; ordinary mutations stay intact; external cancellation works with or without a deadline`,
      );
    } finally {
      await browser.close();
    }
  } finally {
    await server.close();
  }
}

test("Mobile request deadline (Chromium)", async () => {
  test.setTimeout(180_000);
  await runDeadlineChecks("chromium", chromium, browserExecutablePath);
});

test("Mobile request deadline (WebKit)", async () => {
  test.setTimeout(180_000);
  test.skip(
    !existsSync(webkitPath),
    `WebKit runtime is not installed at ${webkitPath}`,
  );
  await runDeadlineChecks("webkit", webkit, webkitPath);
});
