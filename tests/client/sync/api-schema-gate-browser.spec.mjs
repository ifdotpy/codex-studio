import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeEvent,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  readApiSchemaHash,
  expect,
  spawnFixture,
  test,
} from "../playwright.mjs";

async function startColdGateFixture() {
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-schema-cold-gate-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(fixtureLog)));
  });
  return { fixture, url: `http://127.0.0.1:${port}` };
}

test("cold response mismatch shows the gate and update reload reaches rebuild text", async ({
  page,
}) => {
  const { fixture, url } = await startColdGateFixture();
  let apiRequests = 0;
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    apiRequests++;
    if (url.pathname === "/api/sync/stream")
      return route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: apiSchemaHandshakeSse("", { hash: "foreign-schema" }),
      });
    const response = await route.fetch();
    const headers = { ...response.headers() };
    headers[API_SCHEMA_HASH_HEADER.toLowerCase()] = "foreign-schema";
    await route.fulfill({ response, headers });
  });
  try {
    await page.goto(url);
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText:
          "Studio has been updated. Update this tab to continue syncing and sending.",
      }),
    ).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole("button", { name: "Update" })).toBeVisible();
    const quietCount = apiRequests;
    await page.waitForTimeout(8_000);
    assert.equal(apiRequests, quietCount, "API requests stop after mismatch");

    const reload = page.waitForEvent("load");
    await page.getByRole("button", { name: "Update" }).click();
    await reload;
    await expect(
      page.getByText(
        "The installed renderer build does not match the server. Rebuild Studio before continuing.",
      ),
    ).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole("button", { name: "Update" })).toHaveCount(0);
  } finally {
    fixture.kill();
  }
});

test("cold stream handshake mismatch shows the gate when startup responses have no schema hash", async ({
  page,
}) => {
  const { url } = await startColdGateFixture();
  let streams = 0;
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/api/sync/stream") {
      streams++;
      return route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: apiSchemaHandshakeSse("", { hash: "foreign-schema" }),
      });
    }
    const response = await route.fetch();
    const headers = new Headers(response.headers());
    if (url.pathname !== "/api/sync/identity")
      headers.delete(API_SCHEMA_HASH_HEADER);
    await route.fulfill({ response, headers: Object.fromEntries(headers) });
  });
  let apiRequests = 0;
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.startsWith("/api/")) apiRequests++;
  });
  await page.goto(url);
  await expect(
    page.locator('[data-modal-content="true"]').filter({
      hasText:
        "Studio has been updated. Update this tab to continue syncing and sending.",
    }),
  ).toBeVisible({ timeout: 5_000 });
  assert.ok(streams > 0, "the stream handshake caused the mismatch gate");
  await expect(page.getByRole("button", { name: "Update" })).toBeVisible();
  const quietCount = apiRequests;
  await page.waitForTimeout(8_000);
  assert.equal(
    apiRequests,
    quietCount,
    "API requests stop after handshake mismatch",
  );
});

test("matching cold renderer loads normally", async ({ page }) => {
  const { url } = await startColdGateFixture();
  await page.goto(url);
  await expect(page.locator("#message")).toBeVisible({ timeout: 10_000 });
  await expect(page.locator(".startup")).toHaveCount(0);
  await expect(
    page.getByText(
      "Studio has been updated. Update this tab to continue syncing and sending.",
    ),
  ).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Update" })).toHaveCount(0);
});

test("a rollback without a schema header shows Update instead of stale chat data", async ({
  page,
}) => {
  const { fixture, url } = await startColdGateFixture();
  let messageWrites = 0;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (request.method() === "POST" && path === "/api/messages")
      messageWrites++;
    if (path === "/api/sync/identity") {
      const response = await route.fetch();
      const headers = new Headers(response.headers());
      headers.delete(API_SCHEMA_HASH_HEADER);
      return route.fulfill({ response, headers: Object.fromEntries(headers) });
    }
    return route.continue();
  });
  try {
    await page.goto(url);
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText:
          "Studio has been updated. Update this tab to continue syncing and sending.",
      }),
    ).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole("button", { name: "Update" })).toBeVisible();
    assert.equal(messageWrites, 0);
  } finally {
    fixture.kill();
  }
});

test("cold schema identity failure shows a reloadable error", async ({
  page,
}) => {
  const { url } = await startColdGateFixture();
  await page.route("**/api/**", async (route) => {
    if (route.request().headers()[API_SCHEMA_HASH_HEADER.toLowerCase()])
      return route.fulfill({
        status: 503,
        json: {
          error: "Studio API schema identity is unavailable; restart Studio.",
        },
      });
    return route.continue();
  });
  await page.goto(url);
  await expect(
    page.getByText(
      "Studio API schema identity is unavailable; restart Studio.",
    ),
  ).toBeVisible({ timeout: 5_000 });
  await expect(page.getByRole("link", { name: "Reload Studio" })).toBeVisible();
});

test("a warm server rollback preserves the draft and stops the stale stream", async ({
  page,
}) => {
  await page.addInitScript(() => {
    const NativeEventSource = window.EventSource;
    window.studioTestSources = [];
    window.EventSource = class extends NativeEventSource {
      constructor(...args) {
        super(...args);
        window.studioTestSources.push(this);
      }
    };
  });
  const { fixture, url } = await startColdGateFixture();
  try {
    await page.goto(url);
    await expect(page.locator("#message")).toBeVisible({ timeout: 10_000 });
    await page.waitForFunction(() =>
      window.studioTestSources.some(
        (source) => source.readyState === EventSource.OPEN,
      ),
    );
    await page.locator("#message").fill("Preserve this draft after rollback");
    let rejections = 0;
    let identityProbes = 0;
    let messageWrites = 0;
    await page.route("**/api/**", async (route) => {
      const request = route.request();
      const path = new URL(request.url()).pathname;
      if (request.method() === "POST" && path === "/api/messages")
        messageWrites++;
      if (path === "/api/sync/stream") {
        rejections++;
        return route.fulfill({
          status: 426,
          json: { error: "Old sync protocol" },
        });
      }
      const response = await route.fetch();
      const headers = new Headers(response.headers());
      headers.delete(API_SCHEMA_HASH_HEADER);
      if (path === "/api/sync/identity") identityProbes++;
      return route.fulfill({ response, headers: Object.fromEntries(headers) });
    });
    await page.evaluate(() => {
      for (const source of window.studioTestSources) {
        if (source.readyState === EventSource.OPEN)
          source.dispatchEvent(new Event("error"));
      }
    });
    await expect(page.getByRole("button", { name: "Update" })).toBeVisible({
      timeout: 5_000,
    });
    assert.ok(rejections > 0, "the new connection reached the old server");
    assert.ok(identityProbes > 0, "the failed stream rechecked the identity");
    await expect(page.locator("#message")).toHaveValue(
      "Preserve this draft after rollback",
    );
    await expect(
      page.getByRole("button", { name: "Send message" }),
    ).toBeDisabled();
    assert.equal(messageWrites, 0);
  } finally {
    fixture.kill();
  }
});

test("cold three silent stream handshakes escalate to the mismatch gate", async ({
  page,
}) => {
  await page.addInitScript(() => {
    const nativeSetTimeout = window.setTimeout.bind(window);
    window.setTimeout = (handler, timeout, ...args) =>
      nativeSetTimeout(handler, timeout === 15_000 ? 25 : timeout, ...args);
  });
  let streams = 0;
  const { url } = await startColdGateFixture();
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/api/sync/stream") {
      streams++;
      return route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: "",
      });
    }
    return route.continue();
  });
  await page.goto(url);
  await expect(
    page.locator('[data-modal-content="true"]').filter({
      hasText:
        "Studio has been updated. Update this tab to continue syncing and sending.",
    }),
  ).toBeVisible({ timeout: 5_000 });
  assert.ok(streams >= 3, "three silent handshake attempts were made");
  await expect(page.getByRole("button", { name: "Update" })).toBeVisible();
});

test("a response mismatch keeps the loaded transcript and local draft while stopping sync", async ({
  page,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const workerPath = join(root, "web/dist/studio-sw.js");
  const indexPath = join(root, "web/dist/index.html");
  const originalWorker = await readFile(workerPath, "utf8");
  const originalIndex = await readFile(indexPath, "utf8");
  let workerChanged = false;
  let indexChanged = false;
  const browserFailures = [];
  page.on("requestfailed", (request) =>
    browserFailures.push(`${request.url()} ${request.failure()?.errorText}`),
  );
  page.on("response", async (response) => {
    if (response.status() >= 400) {
      let body = "";
      if (new URL(response.url()).pathname === "/api/projects") {
        body = await response.text().catch(() => "<unreadable>");
      }
      browserFailures.push(`${response.status()} ${response.url()} ${body}`);
    }
  });
  const state = await mkdtemp(join(tmpdir(), "studio-schema-gate-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  try {
    await page.addInitScript(() => {
      const key = "studio-api-schema-update-attempted";
      if (sessionStorage.getItem("schema-gate-test-initialized") !== "1") {
        sessionStorage.removeItem(key);
        sessionStorage.setItem("schema-gate-test-initialized", "1");
      }
    });
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    await page.goto(`http://127.0.0.1:${port}`);
    await expect(page.getByRole("alertdialog")).toHaveCount(0);
    await page.evaluate(async () => {
      await navigator.serviceWorker?.ready;
    });
    await page.reload();
    await expect
      .poll(() => page.evaluate(() => !!navigator.serviceWorker?.controller))
      .toBe(true);
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await page.waitForFunction(
      () => document.querySelectorAll("article[data-message]").length > 0,
    );
    const visibleMessages = await page.locator("article[data-message]").count();
    await page.locator("#message").fill("Keep this unsent draft");
    await page.waitForFunction(() =>
      Object.keys(localStorage).some((key) =>
        localStorage.getItem(key)?.includes("Keep this unsent draft"),
      ),
    );

    let totalRequests = 0;
    let apiRequests = 0;
    let mutationRequests = 0;
    const syncRequests = [];
    page.on("request", (request) => {
      totalRequests++;
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) apiRequests++;
      if (request.method() !== "GET" && url.pathname.startsWith("/api/"))
        mutationRequests++;
      if (url.pathname.startsWith("/api/sync/"))
        syncRequests.push(request.url());
    });
    let mismatchEnabled = true;
    let schemaResponses = 0;
    await page.context().route("**/api/**", async (route) => {
      const path = new URL(route.request().url()).pathname;
      if (path === "/api/sync/stream") return route.continue();
      if (!mismatchEnabled) return route.continue();
      const response = await route.fetch();
      const headers = { ...response.headers() };
      if (mismatchEnabled) {
        schemaResponses++;
        headers[API_SCHEMA_HASH_HEADER.toLowerCase()] = "foreign-schema";
      }
      await route.fulfill({
        response,
        headers,
      });
    });
    await page.reload();
    await expect.poll(() => schemaResponses).toBeGreaterThan(0);
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
    await expect(page.locator("#message")).toHaveValue(
      "Keep this unsent draft",
    );
    await expect(
      page.getByRole("button", { name: "Send message" }),
    ).toBeDisabled();
    await expect(page.locator("article[data-message]")).toHaveCount(
      visibleMessages,
    );
    const settledApiRequests = apiRequests;
    const settledRequests = totalRequests;
    const settledSyncRequests = syncRequests.length;
    const settledMutations = mutationRequests;
    await page.waitForTimeout(8_000);
    assert.equal(
      totalRequests,
      settledRequests,
      "No network requests follow the mismatch alert",
    );
    assert.equal(
      apiRequests,
      settledApiRequests,
      "No API requests follow the mismatch alert",
    );
    assert.equal(
      syncRequests.length,
      settledSyncRequests,
      "No sync requests follow the mismatch alert",
    );
    assert.equal(
      mutationRequests,
      settledMutations,
      "No mutations follow the mismatch alert",
    );

    await page.evaluate(() => {
      const serviceWorker = navigator.serviceWorker;
      window.__originalGetRegistration =
        serviceWorker.getRegistration.bind(serviceWorker);
      serviceWorker.getRegistration = async () => undefined;
    });
    await page.getByRole("button", { name: "Update" }).click();
    await expect(
      page.getByRole("alert").filter({
        hasText: "service worker is not registered",
      }),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Update" })).toHaveCount(1);
    expect(
      await page.evaluate(() =>
        sessionStorage.getItem("studio-api-schema-update-attempted"),
      ),
    ).toBeNull();
    await page.evaluate(() => {
      navigator.serviceWorker.getRegistration =
        window.__originalGetRegistration;
      delete window.__originalGetRegistration;
    });

    const networkShellMarker = `schema-gate-network-shell-${Date.now()}`;
    await writeFile(
      workerPath,
      originalWorker.replace(
        'event.respondWith(fetch(new Request(request, { cache: "reload" })));',
        `event.respondWith(
        fetch(new Request(request, { cache: "reload" })).then((response) => {
          const headers = new Headers(response.headers);
          headers.set("x-schema-gate-shell-source", "network");
          return new Response(response.body, {
            status: response.status,
            statusText: response.statusText,
            headers,
          });
        }),
      );`,
      ),
    );
    assert.notEqual(originalWorker, await readFile(workerPath, "utf8"));
    workerChanged = true;
    await writeFile(
      indexPath,
      originalIndex.replace(
        "</head>",
        `<meta name="schema-gate-network-shell" content="${networkShellMarker}"></head>`,
      ),
    );
    indexChanged = true;
    mismatchEnabled = false;
    await page.context().addInitScript(
      (handshakes) => {
        const behavior = sessionStorage.getItem("schema-gate-handshake");
        if (sessionStorage.getItem("schema-gate-strip-api-hash") === "1") {
          const nativeFetch = window.fetch.bind(window);
          window.fetch = async (input, init) => {
            const response = await nativeFetch(input, init);
            const url = new URL(
              input instanceof Request ? input.url : String(input),
              location.href,
            );
            if (!url.pathname.startsWith("/api/")) return response;
            if (url.pathname === "/api/sync/identity") return response;
            const headers = new Headers(response.headers);
            headers.delete("x-studio-api-schema");
            if (init?.method === "GET" || !init?.method)
              window.__schemaGateHeaderlessGets =
                (window.__schemaGateHeaderlessGets || 0) + 1;
            return new Response(response.body, {
              status: response.status,
              statusText: response.statusText,
              headers,
            });
          };
        }
        if (!["match", "mismatch", "silent"].includes(behavior)) return;
        class TestEventSource extends EventTarget {
          static CONNECTING = 0;
          static OPEN = 1;
          static CLOSED = 2;
          readyState = TestEventSource.CONNECTING;
          withCredentials = false;
          onopen = null;
          onerror = null;
          url;
          #closed = false;

          constructor(url) {
            super();
            this.url = new URL(url, location.href).href;
            queueMicrotask(() => {
              if (this.#closed) return;
              this.readyState = TestEventSource.OPEN;
              this.dispatchEvent(new Event("open"));
              if (behavior === "silent") return;
              this.dispatchEvent(
                new MessageEvent("api-schema", {
                  data: JSON.stringify(handshakes[behavior]),
                }),
              );
              if (behavior === "mismatch")
                window.__schemaGateMismatchHandshakes =
                  (window.__schemaGateMismatchHandshakes || 0) + 1;
            });
          }

          dispatchEvent(event) {
            const dispatched = super.dispatchEvent(event);
            this[`on${event.type}`]?.call(this, event);
            return dispatched;
          }

          close() {
            this.#closed = true;
            this.readyState = TestEventSource.CLOSED;
          }
        }
        window.EventSource = TestEventSource;
      },
      {
        match: apiSchemaHandshakeEvent(),
        mismatch: apiSchemaHandshakeEvent({
          hash: "foreign-schema",
          mismatch: true,
        }),
      },
    );
    await page.evaluate(() =>
      sessionStorage.setItem("schema-gate-handshake", "mismatch"),
    );
    await page.evaluate(() =>
      sessionStorage.setItem("schema-gate-strip-api-hash", "1"),
    );
    await page.evaluate(async () => {
      const registration = await navigator.serviceWorker.getRegistration("/");
      registration?.addEventListener("updatefound", () => {
        const worker = registration.installing;
        worker?.addEventListener("statechange", () => {
          if (worker.state === "activated")
            sessionStorage.setItem("schema-gate-worker-activated", "1");
        });
      });
    });
    const updateNavigation = page.waitForRequest((request) => {
      const url = new URL(request.url());
      return (
        request.isNavigationRequest() && url.searchParams.has("studio-update")
      );
    });
    const updateNavigationResponse = page.waitForResponse((response) => {
      const request = response.request();
      return (
        request.isNavigationRequest() &&
        new URL(request.url()).searchParams.has("studio-update")
      );
    });
    await page.getByRole("button", { name: "Update" }).click();
    await updateNavigation;
    expect(
      (await updateNavigationResponse).headers()["x-schema-gate-shell-source"],
    ).toBe("network");
    await expect(page).not.toHaveURL(/studio-update=/, { timeout: 30_000 });
    await expect
      .poll(() =>
        page.evaluate(() =>
          sessionStorage.getItem("schema-gate-worker-activated"),
        ),
      )
      .toBe("1");
    expect(
      await page
        .locator('meta[name="schema-gate-network-shell"]')
        .getAttribute("content"),
    ).toBe(networkShellMarker);
    const activeWorker = await page.evaluate(async () => {
      const registration = await navigator.serviceWorker.getRegistration("/");
      return registration?.active?.state === "activated";
    });
    expect(activeWorker).toBe(true);
    await expect
      .poll(() =>
        page.evaluate(() => window.__schemaGateMismatchHandshakes || 0),
      )
      .toBeGreaterThan(0);
    await expect
      .poll(() => page.evaluate(() => window.__schemaGateHeaderlessGets || 0))
      .toBeGreaterThan(0);
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible({ timeout: 30_000 });
    await expect(page.getByRole("button", { name: "Update" })).toHaveCount(1);
    await writeFile(workerPath, originalWorker);
    workerChanged = false;
    await writeFile(indexPath, originalIndex);
    indexChanged = false;

    await page.evaluate(() =>
      sessionStorage.setItem("schema-gate-handshake", "match"),
    );
    expect(
      await page.evaluate(() =>
        sessionStorage.getItem("studio-api-schema-update-attempted"),
      ),
    ).toBeNull();
    await page.reload();
    await expect(page.getByRole("alertdialog")).toHaveCount(0);
    await expect
      .poll(() =>
        page.evaluate(() =>
          sessionStorage.getItem("studio-api-schema-update-attempted"),
        ),
      )
      .toBeNull();
    await page.evaluate(() =>
      sessionStorage.setItem("schema-gate-handshake", "mismatch"),
    );
    await page.reload();
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Update" })).toHaveCount(1);

    mismatchEnabled = true;
    await page.evaluate(() =>
      sessionStorage.setItem("schema-gate-handshake", "silent"),
    );
    await page.evaluate(() =>
      sessionStorage.setItem("schema-gate-strip-api-hash", "0"),
    );
    await page.reload();
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
    const responseUpdateNavigation = page.waitForRequest((request) => {
      const url = new URL(request.url());
      return (
        request.isNavigationRequest() && url.searchParams.has("studio-update")
      );
    });
    await page.getByRole("button", { name: "Update" }).click();
    await responseUpdateNavigation;
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "The installed renderer build does not match the server",
      }),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Update" })).toHaveCount(0);
  } finally {
    if (workerChanged) await writeFile(workerPath, originalWorker);
    if (indexChanged) await writeFile(indexPath, originalIndex);
    fixture.kill();
  }
});

test("a stream mismatch keeps the transcript and draft while stopping every request", async ({
  page,
}) => {
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-schema-stream-gate-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    await page.goto(`http://127.0.0.1:${port}`);
    await page.evaluate(async () => {
      await navigator.serviceWorker?.ready;
    });
    await page.reload();
    await page.evaluate(() => {
      localStorage.setItem("schema-stream-gate-ready", "1");
    });
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await page.waitForFunction(
      () => document.querySelectorAll("article[data-message]").length > 0,
    );
    const visibleMessages = await page.locator("article[data-message]").count();
    await page.locator("#message").fill("Draft survives a stream mismatch");
    await page.waitForFunction(() =>
      Object.keys(localStorage).some((key) =>
        localStorage.getItem(key)?.includes("Draft survives a stream mismatch"),
      ),
    );
    let totalRequests = 0;
    page.on("request", () => totalRequests++);
    await page.context().route("**/api/sync/stream**", async (route) => {
      await route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: apiSchemaHandshakeSse("", { mismatch: true }),
      });
    });
    await page.reload();
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
    await expect(page.locator("#message")).toHaveValue(
      "Draft survives a stream mismatch",
    );
    await expect(page.locator("article[data-message]")).toHaveCount(
      visibleMessages,
    );
    await expect(
      page.getByRole("button", { name: "Send message" }),
    ).toBeDisabled();
    const settledRequests = totalRequests;
    await page.waitForTimeout(8_000);
    assert.equal(
      totalRequests,
      settledRequests,
      "No requests follow a stream mismatch",
    );
  } finally {
    fixture.kill();
  }
});

test("schema hash change uses a fresh entity cache and keeps the local draft", async ({
  page,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-schema-cache-variant-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: join(root, "web"),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  const originalHash = readApiSchemaHash();
  const updatedHash = `${originalHash.slice(0, -1)}${originalHash.endsWith("0") ? "1" : "0"}`;
  const errors = [];
  const entityFullPulls = [];
  let variantEnabled = false;
  let fixtureUrl = "";
  let fixtureWorkspace = "";
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (
      url.pathname === "/api/sync/pull" &&
      url.searchParams.get("scope") === "state:entities:v1" &&
      url.searchParams.get("after") === "0"
    )
      entityFullPulls.push(url.search);
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    fixtureUrl = `http://127.0.0.1:${port}`;
    await page.route("**/src/generated/apiSchema.ts", async (route) => {
      const response = await route.fetch();
      if (!variantEnabled) return route.fulfill({ response });
      const source = (await response.text()).replace(originalHash, updatedHash);
      await route.fulfill({ response, body: source });
    });
    await page.route("**/api/**", async (route) => {
      const url = new URL(route.request().url());
      const rendererHash = variantEnabled ? updatedHash : originalHash;
      if (url.pathname === "/api/sync/stream") {
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        return route.fulfill({
          status: 200,
          headers: { "content-type": "text/event-stream" },
          body: apiSchemaHandshakeSse(
            protocol3SseEvent("resources", {
              protocol: 3,
              workspaceId: fixtureWorkspace,
              epoch: "schema-cache-variant",
              revision: 1,
              reason: "initial",
              resources,
            }),
            { hash: rendererHash },
          ),
        });
      }
      if (
        route.request().method() === "POST" &&
        url.pathname === "/api/projects"
      )
        return route.fulfill({
          status: 200,
          headers: { [API_SCHEMA_HASH_HEADER]: rendererHash },
          json: { groups: null, revision: 0 },
        });
      if (
        route.request().method() === "POST" &&
        url.pathname === "/api/sync/drafts"
      )
        return route.fulfill({
          status: 200,
          headers: { [API_SCHEMA_HASH_HEADER]: rendererHash },
          json: [],
        });
      const headers = { ...route.request().headers() };
      headers[API_SCHEMA_HASH_HEADER.toLowerCase()] = originalHash;
      headers.origin = fixtureUrl;
      headers.referer = `${fixtureUrl}/`;
      const response = await route.fetch({
        url: `${fixtureUrl}${url.pathname}${url.search}`,
        headers,
      });
      if (url.pathname === "/api/sync/identity")
        fixtureWorkspace = (await response.json()).workspaceId;
      const responseHeaders = new Headers(response.headers());
      responseHeaders.set(API_SCHEMA_HASH_HEADER, rendererHash);
      responseHeaders.delete("x-studio-api-schema-mismatch");
      return route.fulfill({
        response,
        headers: Object.fromEntries(responseHeaders),
      });
    });
    await page.goto(`http://127.0.0.1:${server.httpServer.address().port}`);
    await expect(page.locator("#message")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(".startup")).toHaveCount(0, { timeout: 30_000 });
    const initialCachedStartupMs = await page.evaluate(() => performance.now());
    await expect.poll(() => entityFullPulls.length).toBeGreaterThan(0);
    const initialFullPulls = entityFullPulls.length;
    const initialCheckpoint = await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      return (await db.projections.findOne("state:entities:checkpoint").exec())
        ?.seq;
    });
    await page.reload();
    await expect(page.locator(".startup")).toHaveCount(0, { timeout: 30_000 });
    await expect(page.locator("#message")).toBeVisible();
    const sameHashCachedStartupMs = await page.evaluate(() =>
      performance.now(),
    );
    console.log(
      "Schema cache startup timings (navigation to ready)",
      JSON.stringify({ initialCachedStartupMs, sameHashCachedStartupMs }),
    );
    expect(entityFullPulls).toHaveLength(initialFullPulls);
    expect(
      await page.evaluate(async () => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        return (
          await db.projections.findOne("state:entities:checkpoint").exec()
        )?.seq;
      }),
    ).toBeGreaterThanOrEqual(initialCheckpoint);
    await page
      .locator("#message")
      .fill("Draft survives a schema cache refresh");
    await page.waitForFunction(() =>
      Object.keys(localStorage).some((key) =>
        localStorage
          .getItem(key)
          ?.includes("Draft survives a schema cache refresh"),
      ),
    );

    const fullPullsBeforeUpdate = entityFullPulls.length;
    variantEnabled = true;
    await page.reload();
    await expect(page.locator("#message")).toHaveValue(
      "Draft survives a schema cache refresh",
    );
    await expect(page.locator(".startup")).toHaveCount(0, { timeout: 30_000 });
    await expect
      .poll(() => entityFullPulls.length)
      .toBeGreaterThan(fullPullsBeforeUpdate);
    expect(entityFullPulls.at(-1)).toContain("after=0");
    expect(entityFullPulls).toHaveLength(fullPullsBeforeUpdate + 1);
    expect(errors).toEqual([]);
  } finally {
    await page.unrouteAll({ behavior: "ignoreErrors" });
    await page.close();
    if (fixture.exitCode === null && fixture.signalCode === null)
      fixture.kill();
    await server.close();
  }
});
