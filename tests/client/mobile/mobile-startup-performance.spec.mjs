import {
  test,
  browserExecutablePath,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Production renderer and Runtime with generated history, no live user data.
import assert from "node:assert/strict";
import {
  cp,
  mkdtemp,
  readFile,
  readdir,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { gzipSync } from "node:zlib";
test("Mobile startup performance", { tag: "@performance" }, async () => {
  test.setTimeout(300_000);
  const repo = fileURLToPath(new URL("../../../", import.meta.url));
  const fixtureRepo = process.env.MOBILE_PERF_BACKEND || repo;
  const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
    "playwright-core",
  );
  const useWebKit = process.env.BROWSER === "webkit";
  const browserType = useWebKit ? webkit : chromium;
  const expectCurrentBudgets = process.env.MOBILE_PERF_EXPECT_CAP !== "0";
  const enforceCurrentBudgets = expectCurrentBudgets && !useWebKit;
  const dir = await mkdtemp(join(tmpdir(), "studio-mobile-performance-"));
  const artifactDist = process.env.MOBILE_PERF_DIST || join(repo, "web/dist");
  await cp(artifactDist, join(dir, "dist"), { recursive: true });
  const artifactHtml = await readFile(join(dir, "dist/index.html"), "utf8");
  const artifactBuild = artifactHtml.match(
    /name="studio-build" content="([^"]+)"/,
  )?.[1];
  assert.ok(
    artifactBuild,
    "The production artifact must declare its build identity",
  );
  const proc = spawn(
    process.env.PYTHON || "python3",
    [
      "-B",
      join(fixtureRepo, "tests/mobile-startup-fixture.py"),
      join(dir, "state"),
    ],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, MOBILE_TEST_DIST: join(dir, "dist") },
    },
  );
  let log = "",
    browser,
    page;
  proc.stderr.on("data", (data) => {
    log += data;
  });
  const errors = [];
  const failedRequests = [];
  const responseStatuses = [];
  const transcriptResponses = [];
  const consoleErrors = [];
  let scrollDebug = null;
  let scrollExpectation = null;
  let requests = [];
  const measurements = {
    artifactBuild,
    browser: useWebKit ? "WebKit" : "Chromium",
    network: {
      downloadBytesPerSecond: useWebKit ? null : 200000,
      uploadBytesPerSecond: useWebKit ? null : 100000,
      latencyMs: useWebKit ? null : 100,
      cpuRate: useWebKit ? null : 4,
      note: useWebKit
        ? "WebKit throttles asset responses; fixture API loopback and CPU are unthrottled"
        : "Chromium uses 4x CPU slowdown and Fast 4G throttling",
    },
  };
  const assets = await readdir(join(dir, "dist/assets"));
  const scriptAssets = assets.filter((name) => name.endsWith(".js"));
  const scriptSizes = await Promise.all(
    scriptAssets.map(async (name) => ({
      name,
      bytes: (await stat(join(dir, "dist/assets", name))).size,
      gzipBytes: gzipSync(await readFile(join(dir, "dist/assets", name)), {
        level: 3,
      }).length,
    })),
  );
  const entryHtml = await readFile(join(dir, "dist/index.html"), "utf8");
  const initialNames = [
    ...entryHtml.matchAll(/(?:src|href)="\.\/assets\/([^"?#]+\.js)"/g),
  ].map((m) => m[1]);
  measurements.bundle = {
    totalJsBytes: scriptSizes.reduce((sum, item) => sum + item.bytes, 0),
    totalJsGzipBytes: scriptSizes.reduce(
      (sum, item) => sum + item.gzipBytes,
      0,
    ),
    initialJsAssets: initialNames,
    initialJsBytes: scriptSizes
      .filter((item) => initialNames.includes(item.name))
      .reduce((sum, item) => sum + item.bytes, 0),
    initialJsGzipBytes: scriptSizes
      .filter((item) => initialNames.includes(item.name))
      .reduce((sum, item) => sum + item.gzipBytes, 0),
  };
  try {
    const origin = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) =>
        resolve(`http://127.0.0.1:${Number(String(data).trim())}`),
      );
      proc.once("exit", () => reject(new Error(log)));
    });
    const fixture = JSON.parse(
      await readFile(join(dir, "state/performance-fixture.json"), "utf8"),
    );
    const entityState = await readTestState(origin);
    const compactState = {
      threads: entityState.threads,
      chats: entityState.chats,
      runtime: {
        agents: entityState.runtime.agents,
        rooms: entityState.runtime.rooms,
        tasks: entityState.runtime.tasks,
        monitors: entityState.runtime.monitors,
        complaints: entityState.runtime.complaints,
        requests: entityState.runtime.requests,
        projects: entityState.runtime.projects,
        peerTeams: entityState.runtime.peerTeams,
        events: entityState.runtime.events,
      },
    };
    const full = JSON.stringify(entityState);
    const compact = JSON.stringify(compactState);
    const selected = compactState.threads.find(
      (agent) => agent.id === fixture.lead,
    );
    const root = selected?.isLead ? selected.id : selected?.rootId;
    const backgroundHistoryTargets = compactState.threads.filter(
      (agent) =>
        agent.source === "managed" &&
        agent.id !== fixture.lead &&
        !agent.archived &&
        (agent.isLead || (!!root && agent.rootId === root)),
    ).length;
    const maxIdlePulls = 18; // Five sync scopes, plus 12 bounded retries and one chat pull.
    Object.assign(measurements, {
      fixture,
      backgroundHistoryTargets,
      fullStateBytes: Buffer.byteLength(full),
      fullStateGzipBytes: gzipSync(full, { level: 3 }).length,
      compactStateBytes: Buffer.byteLength(compact),
      compactStateGzipBytes: gzipSync(compact, { level: 3 }).length,
    });
    assert.ok(
      measurements.fullStateGzipBytes > 4_000_000,
      "Fixture history must remain large after compression",
    );
    assert.ok(
      !compactState.runtime.work,
      "The chat projection excludes retained work history",
    );
    browser = await browserType.launch({
      headless: true,
      ...(browserType === chromium
        ? { executablePath: browserExecutablePath }
        : {}),
    });
    const context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      screen: { width: 390, height: 844 },
      deviceScaleFactor: 3,
      isMobile: true,
      hasTouch: true,
    });
    page = await context.newPage();
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("console", (message) => {
      if (message.type() === "error") consoleErrors.push(message.text());
    });
    await page.addInitScript((lead) => {
      localStorage.setItem("codex-mobile-opened", JSON.stringify(lead));
      const NativeEventSource = window.EventSource;
      window.performanceStreams = [];
      window.mobileLongTasks = [];
      if ("PerformanceObserver" in window) {
        try {
          new PerformanceObserver((list) =>
            window.mobileLongTasks.push(
              ...list.getEntries().map((entry) => ({
                startTime: entry.startTime,
                duration: entry.duration,
              })),
            ),
          ).observe({ type: "longtask", buffered: true });
        } catch {
          /* WebKit does not expose longtask entries on all versions. */
        }
      }
      window.EventSource = class extends NativeEventSource {
        constructor(url, options) {
          super(url, options);
          this.testOpen = true;
          window.performanceStreams.push(this);
        }
        close() {
          this.testOpen = false;
          super.close();
        }
      };
    }, fixture.lead);
    if (useWebKit) {
      await page.route("**/*", async (route) => {
        const url = new URL(route.request().url());
        const isStream = url.pathname === "/api/sync/stream";
        if (isStream || url.pathname.startsWith("/api/"))
          return route.continue();
        const response = await route.fetch();
        const body = await response.body();
        const bytes = body.byteLength;
        await new Promise((resolve) =>
          setTimeout(resolve, 100 + Math.ceil((bytes / 200000) * 1000)),
        );
        await route.fulfill({ response, body });
      });
    }
    page.on("request", (request) =>
      requests.push({
        path: new URL(request.url()).pathname + new URL(request.url()).search,
        at: Date.now(),
        method: request.method(),
      }),
    );
    page.on("requestfailed", (request) =>
      failedRequests.push({
        url: new URL(request.url()).pathname + new URL(request.url()).search,
        error: request.failure()?.errorText || "unknown",
      }),
    );
    page.on("response", async (response) => {
      const url = new URL(response.url());
      const path = url.pathname + url.search;
      responseStatuses.push({ url: path, status: response.status() });
      if (url.pathname !== "/api/transcript/page") return;
      try {
        const body = await response.json();
        transcriptResponses.push({
          url: path,
          status: response.status(),
          itemCount: body.items?.length ?? null,
          firstId: body.items?.[0]?.id ?? null,
          lastId: body.items?.at(-1)?.id ?? null,
          nextCursor: body.nextCursor ?? null,
          historyVersion: body.historyVersion ?? null,
          error: body.error ?? null,
        });
      } catch {
        /* Ignore streaming and non-JSON responses. */
      }
    });
    let cdp;
    if (browserType === chromium) {
      cdp = await context.newCDPSession(page);
      await cdp.send("Network.enable");
      await cdp.send("Network.emulateNetworkConditions", {
        offline: false,
        latency: 100,
        downloadThroughput: 200000,
        uploadThroughput: 100000,
      });
      await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
    }
    const started = Date.now();
    await page.goto(origin, { waitUntil: "domcontentloaded" });
    await page.locator("#message").waitFor({ timeout: 90000 });
    await page.waitForFunction(
      () => !document.querySelector("#message")?.disabled,
    );
    measurements.coldComposerMs = Date.now() - started;
    await page.locator("#sidebar-toggle").click();
    await page
      .locator("#chat-list [data-chat]")
      .first()
      .waitFor({ state: "visible" });
    await page.locator("#chat-search").fill("Release lead");
    await page
      .locator(`#chat-list [data-chat="${fixture.lead}"]`)
      .waitFor({ state: "visible" });
    measurements.coldUsableChatListMs = Date.now() - started;
    if (expectCurrentBudgets) {
      assert.equal(
        requests.filter((request) => request.path === "/api/sync/pull").length,
        0,
        "Startup does not download a full snapshot",
      );
      assert.ok(
        requests.some((request) =>
          request.path.includes("scope=state%3Aentities%3Av1"),
        ),
        "Startup pulls the incremental entity projection",
      );
      assert.equal(
        requests.filter(
          (request) =>
            request.path.startsWith("/api/sync/pull?") &&
            new URL(request.path, origin).searchParams.get("scope") === "state",
        ).length,
        0,
        "Startup never pulls the full state projection",
      );
    }
    await page.evaluate(() => navigator.serviceWorker.ready);
    const draft =
      "Retain this exact mobile performance draft after the app closes.";
    await page.locator("#message").fill(draft);
    await page.waitForFunction((text) => {
      for (let index = 0; index < localStorage.length; index++) {
        const key = localStorage.key(index);
        if (
          key.startsWith("codex-chat-draft:") &&
          localStorage.getItem(key).includes(text)
        )
          return true;
      }
      return false;
    }, draft);
    const idleAt = requests.length;
    await page.waitForTimeout(6500);
    const idleRequests = requests
      .slice(idleAt)
      .filter((request) => request.path.startsWith("/api/"));
    measurements.idleRequests = idleRequests.map((request) => request.path);
    measurements.observedTranscriptScopes = [
      ...new Set(
        requests.flatMap((request) => {
          if (!request.path.startsWith("/api/sync/pull?")) return [];
          const scope = new URL(request.path, origin).searchParams.get("scope");
          return scope?.startsWith("transcript:") ? [scope] : [];
        }),
      ),
    ];
    if (expectCurrentBudgets)
      assert.ok(
        measurements.observedTranscriptScopes.length <= 3,
        `Mobile history prefetch opened ${measurements.observedTranscriptScopes.length} transcript scopes`,
      );
    measurements.openEntitySyncStreams = await page.evaluate(
      () =>
        window.performanceStreams.filter(
          (stream) =>
            stream.testOpen &&
            new URL(stream.url).pathname === "/api/sync/stream" &&
            new URL(stream.url).searchParams.get("scope") ===
              "state:entities:v1",
        ).length,
    );
    if (expectCurrentBudgets) {
      assert.equal(
        measurements.openEntitySyncStreams,
        1,
        "The full app retains one shared entity sequence stream",
      );
      assert.ok(
        idleRequests.filter((request) => request.path === "/api/session")
          .length <= 1,
        "Idle credentials do not poll every 1.6 seconds",
      );
    }
    // New cursors can reflect one-time prefetch invalidations, not idle polling.
    // Count repeated requests at the same cursor across every scope.
    const pullsByCursor = new Map();
    for (const request of idleRequests) {
      if (!request.path.startsWith("/api/sync/pull?")) continue;
      pullsByCursor.set(
        request.path,
        (pullsByCursor.get(request.path) || 0) + 1,
      );
    }
    const repeatedPulls = [...pullsByCursor.values()].reduce(
      (total, count) => total + count - 1,
      0,
    );
    if (expectCurrentBudgets) {
      assert.ok(
        repeatedPulls <= 12,
        "Repeated pulls at the same cursor remain bounded",
      );
      assert.ok(
        idleRequests.filter((request) =>
          request.path.startsWith("/api/sync/pull?"),
        ).length <= maxIdlePulls,
        `Idle projection pulls exceed 6 one-time pulls plus 12 repeats`,
      );
    }
    await page.waitForFunction(
      async (entityCount) => {
        const databases = await indexedDB.databases();
        for (const info of databases) {
          if (!info.name) continue;
          const db = await new Promise((resolve, reject) => {
            const request = indexedDB.open(info.name);
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
          });
          if (!db.objectStoreNames.contains("projections")) {
            db.close();
            continue;
          }
          const rows = await new Promise((resolve, reject) => {
            const result = [];
            const request = db
              .transaction("projections", "readonly")
              .objectStore("projections")
              .openCursor();
            request.onsuccess = () => {
              const cursor = request.result;
              if (!cursor) return resolve(result);
              result.push(cursor.value);
              cursor.continue();
            };
            request.onerror = () => reject(request.error);
          });
          db.close();
          const complete = rows.some(
            (row) => row.id === "state:entities:complete" && !row._deleted,
          );
          const entities = rows.filter(
            (row) => row.id?.startsWith("entity:") && !row._deleted,
          ).length;
          if (complete || entities >= entityCount) return true;
        }
        return false;
      },
      fixture.entityCount,
      { timeout: 90000 },
    );
    measurements.fullEntitySyncMs = Date.now() - started;
    measurements.initialSyncTransfer = await page.evaluate(() => {
      const resources = performance
        .getEntriesByType("resource")
        .filter((entry) => {
          const url = new URL(entry.name);
          return (
            url.origin === location.origin && url.pathname === "/api/sync/pull"
          );
        })
        .map((entry) => {
          const url = new URL(entry.name);
          return {
            path: url.pathname,
            scope: url.searchParams.get("scope"),
            transferBytes: entry.transferSize || null,
            encodedBodyBytes: entry.encodedBodySize || null,
            decodedBodyBytes: entry.decodedBodySize || null,
          };
        });
      return {
        requests: resources.length,
        measuredTransferBytes: resources.reduce(
          (sum, entry) => sum + (entry.transferBytes || 0),
          0,
        ),
        measuredEncodedBodyBytes: resources.reduce(
          (sum, entry) => sum + (entry.encodedBodyBytes || 0),
          0,
        ),
        resources,
      };
    });
    const warmStarted = Date.now();
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForFunction(
      (text) => document.querySelector("#message")?.value === text,
      draft,
      { timeout: 60000 },
    );
    measurements.warmDraftMs = Date.now() - warmStarted;
    if (enforceCurrentBudgets)
      assert.ok(
        measurements.warmDraftMs < 5000,
        `Cached chat and exact draft exceed 5 seconds: ${measurements.warmDraftMs} ms`,
      );
    if (enforceCurrentBudgets)
      assert.ok(
        measurements.coldUsableChatListMs < 18000,
        `Cold chat list exceeds 18 seconds: ${measurements.coldUsableChatListMs} ms`,
      );
    const cancelledRequestErrors = useWebKit
      ? errors.filter((error) =>
          failedRequests.some(
            (request) =>
              request.error === "cancelled" &&
              request.url.startsWith("/api/panel?") &&
              error.includes(request.url),
          ),
        )
      : [];
    measurements.cancelledPanelErrorsOnReload = cancelledRequestErrors;
    assert.deepEqual(
      errors.filter((error) => !cancelledRequestErrors.includes(error)),
      [],
    );

    const waitUntil = async (predicate, argument, label, timeout = 30000) => {
      try {
        await page.waitForFunction(predicate, argument, { timeout });
      } catch {
        throw new Error(`Timed out waiting for ${label}`);
      }
    };
    const openChat = async (id, name) => {
      await page.locator("#sidebar-toggle").click();
      await page.locator("#chat-search").fill(name);
      const row = page.locator(`#chat-list [data-chat="${id}"]`);
      await row.waitFor({ state: "visible" });
      const at = Date.now();
      await row.click();
      await waitUntil(
        (expected) =>
          document
            .querySelector("#conversation-title")
            ?.textContent?.includes(expected),
        name,
        `chat ${name}`,
      );
      await page.locator("#message").waitFor({ state: "visible" });
      return Date.now() - at;
    };
    const switchTargets = fixture.switchTargets;
    assert.equal(
      switchTargets.length,
      2,
      "The fixture provides two switch chats",
    );
    const target = switchTargets[0];
    if (target) {
      measurements.openChatMs = await openChat(target.id, target.name);
      const second = switchTargets[1];
      if (second)
        measurements.switchChatMs = await openChat(second.id, second.name);
      const sendText = "Mobile performance fixture send visible check.";
      await page.locator("#message").fill(sendText);
      const sendAt = Date.now();
      await page.locator("#send").click();
      await page
        .locator("#messages [data-message]")
        .filter({ hasText: sendText })
        .waitFor({ state: "visible" });
      measurements.sendToVisibleMs = Date.now() - sendAt;
    }
    const leadName = await openChat(fixture.lead, "Release lead");
    measurements.returnToLongChatMs = leadName;
    const scrollStarted = Date.now();
    const earlier = page.locator("#earlier-messages");
    await earlier.waitFor({ state: "visible", timeout: 30000 });
    let scrollPagesLoaded = 0;
    let maxVisibleMessages = 0;
    const scrollPageTimes = [];
    const observedMessageIds = new Set();
    while ((await earlier.count()) && observedMessageIds.size <= 1000) {
      await earlier.scrollIntoViewIfNeeded();
      const messageRows = page.locator("#messages [data-message]");
      maxVisibleMessages = Math.max(
        maxVisibleMessages,
        await messageRows.count(),
      );
      for (const id of await messageRows.evaluateAll((rows) =>
        rows.map((row) => row.getAttribute("data-message")),
      ))
        if (id) observedMessageIds.add(id);
      const previousTop = await messageRows
        .first()
        .getAttribute("data-message");
      const pageResponsePromise = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/transcript/page" &&
          new URL(response.url()).searchParams.has("before"),
        { timeout: 30000 },
      );
      const pageStarted = Date.now();
      await earlier.click();
      const pageResponse = await pageResponsePromise;
      const pageData = await pageResponse.json();
      assert.equal(
        pageData.error,
        undefined,
        "Older transcript page has no API error",
      );
      assert.ok(pageData.items?.length, "Older transcript page returns items");
      await page.locator("#messages").evaluate((root) => {
        root.scrollTop = 0;
        root.dispatchEvent(new Event("scroll", { bubbles: true }));
      });
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      const returnedPageIds = pageData.items.slice(0, 5).map((item) => item.id);
      scrollExpectation = {
        previousTop,
        expectedFirstIds: returnedPageIds,
        cursor: new URL(pageResponse.url()).searchParams.get("before"),
      };
      await page.waitForFunction(
        ({ id, expectedIds }) => {
          const button = document.querySelector("#earlier-messages");
          const rows = [
            ...document.querySelectorAll("#messages [data-message]"),
          ];
          const ids = new Set(
            rows.map((row) => row.getAttribute("data-message")),
          );
          const first = rows[0]?.getAttribute("data-message");
          return (
            !button ||
            (expectedIds.some((messageId) => ids.has(messageId)) &&
              !button.disabled &&
              first !== id)
          );
        },
        { id: previousTop, expectedIds: returnedPageIds },
        { timeout: 30000 },
      );
      scrollDebug = null;
      scrollExpectation = null;
      scrollPagesLoaded++;
      scrollPageTimes.push(Date.now() - pageStarted);
      console.log(
        `SCROLL_PAGE ${scrollPagesLoaded}: ${scrollPageTimes.at(-1)} ms`,
      );
    }
    for (const id of await page
      .locator("#messages [data-message]")
      .evaluateAll((rows) =>
        rows.map((row) => row.getAttribute("data-message")),
      ))
      if (id) observedMessageIds.add(id);
    measurements.longChatMessagesRendered = await page
      .locator("#messages [data-message]")
      .count();
    measurements.longChatMessagesObserved = observedMessageIds.size;
    const expectedFixtureMessages = Array.from(
      { length: 1500 },
      (_, index) =>
        `${fixture.lead}:mobile-perf-message-${String(index).padStart(4, "0")}`,
    );
    const missingFixtureMessages = expectedFixtureMessages.filter(
      (id) => !observedMessageIds.has(id),
    );
    measurements.fixtureTranscriptMessageCoverage = {
      expected: expectedFixtureMessages.length,
      observed: expectedFixtureMessages.length - missingFixtureMessages.length,
    };
    measurements.longChatMaxVisibleMessages = maxVisibleMessages;
    measurements.longChatPagesScrolled = scrollPagesLoaded;
    measurements.longChatScrollPageMs = scrollPageTimes;
    measurements.longChatScrollMs = Date.now() - scrollStarted;
    assert.ok(
      scrollPagesLoaded > 0,
      "The long transcript must load older pages while scrolling",
    );
    assert.ok(
      measurements.longChatMessagesObserved > 1000,
      `The scrolling run observed only ${measurements.longChatMessagesObserved} messages`,
    );
    if (cdp) await cdp.send("HeapProfiler.collectGarbage");
    measurements.jsHeapBytes = cdp
      ? (await cdp.send("Runtime.getHeapUsage")).usedSize
      : await page.evaluate(() => performance.memory?.usedJSHeapSize ?? null);
    measurements.longTasks = await page.evaluate(() => ({
      count: window.mobileLongTasks.length,
      totalMs: window.mobileLongTasks.reduce(
        (sum, task) => sum + task.duration,
        0,
      ),
      maxMs: Math.max(
        0,
        ...window.mobileLongTasks.map((task) => task.duration),
      ),
    }));
    measurements.storage = await page.evaluate(async () => {
      const databases = await indexedDB.databases();
      let objects = 0;
      let serializedBytes = 0;
      let transcriptDocuments = 0;
      for (const info of databases) {
        if (!info.name) continue;
        const db = await new Promise((resolve, reject) => {
          const request = indexedDB.open(info.name);
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        });
        for (const storeName of Array.from(db.objectStoreNames)) {
          const store = db
            .transaction(storeName, "readonly")
            .objectStore(storeName);
          const rows = await new Promise((resolve, reject) => {
            const values = [];
            const request = store.openCursor();
            request.onsuccess = () => {
              const cursor = request.result;
              if (!cursor) return resolve(values);
              values.push(cursor.value);
              cursor.continue();
            };
            request.onerror = () => reject(request.error);
          });
          objects += rows.length;
          serializedBytes += rows.reduce(
            (sum, row) => sum + JSON.stringify(row).length * 2,
            0,
          );
          transcriptDocuments += rows.filter((row) =>
            String(row.id).startsWith("transcript:"),
          ).length;
        }
        db.close();
      }
      const estimate = await navigator.storage?.estimate?.();
      return {
        databaseNames: databases.map((row) => row.name),
        databases: databases.length,
        objects,
        serializedBytes,
        transcriptDocuments,
        originUsageBytes: estimate?.usage ?? null,
        originQuotaBytes: estimate?.quota ?? null,
      };
    });
    measurements.requests = requests;
    await writeFile(
      join(dir, "result.json"),
      JSON.stringify(measurements, null, 2),
    );
    console.log(
      `PASS: mobile startup ${measurements.coldComposerMs} ms, cached draft ${measurements.warmDraftMs} ms, full state ${measurements.fullStateGzipBytes} bytes gzip, compact state ${measurements.compactStateGzipBytes} bytes gzip, one sync stream. Evidence: ${dir}`,
    );
  } catch (error) {
    try {
      scrollDebug = await page?.evaluate(() => ({
        firstMessage: document
          .querySelector("#messages [data-message]")
          ?.getAttribute("data-message"),
        messageCount: document.querySelectorAll("#messages [data-message]")
          .length,
        buttonText: document.querySelector("#earlier-messages")?.textContent,
        buttonDisabled: document.querySelector("#earlier-messages")?.disabled,
        visibleText: document
          .querySelector("#messages")
          ?.innerText?.slice(0, 300),
        draftValue: document.querySelector("#message")?.value,
        title: document.querySelector("#conversation-title")?.textContent,
        longTasks: window.mobileLongTasks?.length,
        maxLongTaskMs: Math.max(
          0,
          ...(window.mobileLongTasks || []).map((task) => task.duration),
        ),
        heapBytes: performance.memory?.usedJSHeapSize ?? null,
        notices: [...document.querySelectorAll("[role=alert], .notice")].map(
          (node) => node.textContent,
        ),
      }));
    } catch {
      /* The page may have closed after a browser error. */
    }
    await page?.screenshot({ path: join(dir, "failure.png") });
    await writeFile(
      join(dir, "failure.json"),
      JSON.stringify(
        {
          measurements,
          errors,
          consoleErrors,
          failedRequests,
          transcriptResponses,
          responseStatuses,
          requests,
          scrollDebug,
          scrollExpectation,
          log,
        },
        null,
        2,
      ),
    );
    console.error("Evidence:", dir);
    throw error;
  } finally {
    await page?.unrouteAll({ behavior: "ignoreErrors" });
    await browser?.close();
  }
});
