// Real production renderer, service worker, browser cache and isolated backend.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer, request as httpRequest } from "node:http";
import { mkdtemp, readFile, cp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { gzipSync } from "node:zlib";
const repo = fileURLToPath(new URL("../", import.meta.url));
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const useWebKit = process.env.BROWSER === "webkit";
const dir = await mkdtemp(join(tmpdir(), "studio-mobile-loading-"));
await cp(join(repo, "web/dist"), join(dir, "dist"), { recursive: true });
const html = await readFile(join(dir, "dist/index.html"), "utf8");
const worker = await readFile(join(dir, "dist/studio-sw.js"), "utf8");
const config = JSON.parse(worker.match(/^self\.STUDIO_SHELL = (.*);/)[1]);
const nextBuild = config.build + "-update";
const bootstrapPath = config.initial.find((path) =>
  path.includes("/studio-startup-"),
);
const cssPath = config.initial.find((path) => /\/main-[^/]+\.css$/.test(path));
const replacements = [
  [bootstrapPath, bootstrapPath.replace(".js", "-update.js")],
  [cssPath, cssPath.replace(".css", "-update.css")],
];
const bytes = { decoded: 0, gzip: 0, requests: config.initial.length };
for (const path of config.initial) {
  const data = await readFile(
    join(
      dir,
      "dist",
      path === "/"
        ? "index.html"
        : replacements.find(([, next]) => next === path)?.[0] || path,
    ),
  );
  bytes.decoded += data.length;
  bytes.gzip += gzipSync(data, { level: 3 }).length;
}
assert.ok(
  bytes.gzip < 650000,
  `Initial shell exceeds 650 KB gzip: ${bytes.gzip}`,
);
assert.ok(
  !config.initial.some((path) => /mermaid|xterm|panel-ui/.test(path)),
  "Optional engines are not precached",
);
assert.ok(!config.allowed.some((path) => path.startsWith("/api/")));

const proc = spawn(
  process.env.PYTHON || "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), join(dir, "state")],
  {
    stdio: ["ignore", "pipe", "pipe"],
  },
);
let log = "",
  browser,
  server;
proc.stderr.on("data", (data) => (log += data));
try {
  const backend = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (data) =>
      resolve(`http://127.0.0.1:${Number(String(data).trim())}`),
    );
    proc.once("exit", () => reject(new Error(log)));
  });
  let version = 1,
    documentDelay = 0,
    rejectEntry = false;
  let laptopDisconnected = false;
  const requests = [];
  const activations = new Set();
  server = createServer(async (request, response) => {
    const path = new URL(request.url, "http://localhost").pathname;
    if (laptopDisconnected) {
      request.socket.destroy();
      return;
    }
    if (path === "/test-worker-activated") {
      activations.add(
        new URL(request.url, "http://localhost").searchParams.get("build"),
      );
      response.writeHead(204);
      response.end();
      return;
    }
    if (path.startsWith("/api/")) {
      const proxy = httpRequest(
        backend + request.url,
        {
          method: request.method,
          headers: {
            ...request.headers,
            host: new URL(backend).host,
            origin: backend,
            referer: backend + "/",
          },
        },
        (upstream) => {
          response.writeHead(upstream.statusCode, upstream.headers);
          upstream.pipe(response);
        },
      );
      proxy.on("error", () => {
        response.writeHead(502);
        response.end();
      });
      response.on("close", () => proxy.destroy());
      request.pipe(proxy);
      return;
    }
    try {
      let data = await readFile(
        join(
          dir,
          "dist",
          path === "/"
            ? "index.html"
            : replacements.find(([, next]) => next === path)?.[0] || path,
        ),
      );
      if (
        version >= 2 &&
        (path === "/" || path === "/index.html" || path === "/studio-sw.js")
      ) {
        let next = String(data).replaceAll(config.build, nextBuild);
        for (const [previous, updated] of replacements)
          next = next.replaceAll(previous, updated);
        data = Buffer.from(next);
      }
      if (path === replacements[0][1])
        data = Buffer.from(
          String(data) + "\nwindow.studioTestBootstrap = 'updated';",
        );
      if (path === replacements[1][1])
        data = Buffer.from(
          String(data) + "\n:root { --studio-test-build: updated; }",
        );
      if (
        version === 3 &&
        (path === "/" || path === "/index.html" || path === "/studio-sw.js")
      ) {
        data = Buffer.from(
          String(data).replaceAll(nextBuild, nextBuild + "-broken"),
        );
        if (path === "/studio-sw.js")
          data = Buffer.from(
            String(data).replace(
              '"initial":[',
              '"initial":["/assets/unavailable-build.js",',
            ),
          );
      }
      if (rejectEntry && /\/main-[^/]+\.js$/.test(path)) {
        response.writeHead(503);
        response.end();
        return;
      }
      if (path === "/studio-sw.js")
        data = Buffer.from(
          String(data) +
            `\nself.addEventListener("activate", event => event.waitUntil(fetch("/test-worker-activated?build=" + self.STUDIO_SHELL.build)));`,
        );
      const type = path.endsWith(".js")
        ? "text/javascript"
        : path.endsWith(".css")
          ? "text/css"
          : path.endsWith(".png")
            ? "image/png"
            : path.endsWith(".webmanifest")
              ? "application/manifest+json"
              : "text/html";
      const body = gzipSync(data, { level: 3 });
      requests.push({ path, bytes: body.length });
      const send = () => {
        if (response.destroyed) return;
        response.writeHead(200, {
          "Content-Type": type,
          "Content-Encoding": "gzip",
          "Content-Length": body.length,
          "Cache-Control": path.startsWith("/assets/")
            ? "private, max-age=31536000, immutable"
            : "no-cache",
          "Content-Security-Policy":
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; worker-src 'self'; frame-src 'self' blob:",
        });
        response.end(body);
      };
      if (path === "/" && documentDelay) setTimeout(send, documentDelay);
      else send();
    } catch {
      response.writeHead(404);
      response.end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  browser = useWebKit
    ? await webkit.launch({ headless: true })
    : await chromium.launch({
        headless: true,
        executablePath:
          process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      });
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  let page = await context.newPage();
  const errors = [];
  const offlineNetworkErrors = [];
  let lastNavigation = 0;
  const reload = async () => {
    lastNavigation = Date.now();
    await page.reload({ waitUntil: "domcontentloaded" });
  };
  page.on("pageerror", (error) => {
    if (
      useWebKit &&
      (laptopDisconnected || Date.now() - lastNavigation < 2000) &&
      /^\/127\.0\.0\.1:\d+\/api\/.* due to access control checks\.$/.test(
        error.message,
      )
    )
      offlineNetworkErrors.push(error.message);
    else errors.push(error.message);
  });
  const cdp = useWebKit ? null : await context.newCDPSession(page);
  await cdp?.send("Network.enable");
  await cdp?.send("Network.emulateNetworkConditions", {
    offline: false,
    latency: 150,
    downloadThroughput: 131072,
    uploadThroughput: 65536,
  });
  const start = Date.now();
  await page.goto(origin, { waitUntil: "domcontentloaded" });
  await page.locator("textarea").first().waitFor({ timeout: 20000 });
  const coldMs = Date.now() - start;
  await page.evaluate(() => navigator.serviceWorker.ready);
  assert.deepEqual(errors, [], "Initial online startup has no script errors");
  const initialNetwork = requests.filter((request) =>
    config.initial.includes(request.path),
  );
  // Chromium reuses the document HTTP cache. This WebKit context has a separate
  // worker HTTP cache; its first install transfers each asset at most once more.
  for (const path of config.initial.filter((path) =>
    path.startsWith("/assets/"),
  ))
    assert.ok(
      initialNetwork.filter((request) => request.path === path).length <=
        (useWebKit ? 2 : 1),
      `Initial transfer repeated beyond the browser cache boundary: ${path}`,
    );
  await cdp?.send("Network.emulateNetworkConditions", {
    offline: false,
    latency: 0,
    downloadThroughput: -1,
    uploadThroughput: -1,
  });
  const draft = "Keep this mobile draft through an offline restart and update.";
  await page.locator("textarea").first().fill(draft);
  await reload();
  await page.waitForFunction(() => !!navigator.serviceWorker.controller);
  await page.waitForFunction(
    (text) => document.querySelector("textarea")?.value === text,
    draft,
  );
  documentDelay = 4000;
  const warmStart = Date.now();
  await reload();
  await page.waitForFunction(
    (text) => document.querySelector("textarea")?.value === text,
    draft,
  );
  const warmMs = Date.now() - warmStart;
  assert.ok(warmMs < 3000, `Sleeping Mac held the cached shell: ${warmMs} ms`);
  documentDelay = 0;

  version = 2;
  await page.evaluate(async () =>
    (await navigator.serviceWorker.getRegistration()).update(),
  );
  await page.waitForFunction(
    async () => !!(await navigator.serviceWorker.getRegistration()).waiting,
  );
  assert.equal(
    await page.locator('meta[name="studio-build"]').getAttribute("content"),
    config.build,
    "Installing an update does not replace the active document",
  );
  await reload();
  assert.equal(
    await page.locator('meta[name="studio-build"]').getAttribute("content"),
    nextBuild,
    "Online reload does not force an old UI",
  );
  await page.waitForFunction(
    (text) => document.querySelector("textarea")?.value === text,
    draft,
  );
  assert.ok(
    await page.evaluate(
      async () => !!(await navigator.serviceWorker.getRegistration()).waiting,
    ),
    "The update does not claim a live client",
  );
  laptopDisconnected = true;
  await reload();
  assert.equal(
    await page.locator('meta[name="studio-build"]').getAttribute("content"),
    nextBuild,
    "Offline reload does not roll back the displayed build while its worker waits",
  );
  assert.equal(
    await page.evaluate(() => window.studioTestBootstrap),
    "updated",
    "The newer cached JavaScript loads under the previous worker",
  );
  assert.equal(
    await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--studio-test-build")
        .trim(),
    ),
    "updated",
    "The newer cached stylesheet loads under the previous worker",
  );
  assert.equal(
    await page.evaluate(() => navigator.onLine),
    true,
    "The laptop is unreachable despite a connected phone network",
  );
  laptopDisconnected = false;
  await reload();
  await page.waitForFunction(
    (text) => document.querySelector("textarea")?.value === text,
    draft,
  );
  const nextWorker = useWebKit
    ? null
    : (
        await Promise.all(
          context.serviceWorkers().map(async (worker) => ({
            worker,
            build: await worker.evaluate(() => self.STUDIO_SHELL.build),
          })),
        )
      ).find((entry) => entry.build === nextBuild).worker;
  lastNavigation = Date.now();
  await page.close();
  const activationDeadline = Date.now() + 15000;
  while (!activations.has(nextBuild) && Date.now() < activationDeadline)
    await new Promise((resolve) => setTimeout(resolve, 25));
  assert.ok(
    activations.has(nextBuild),
    "The waiting worker activates after the last page closes",
  );
  page = await context.newPage();
  await page.goto(origin, { waitUntil: "domcontentloaded" });
  await page.waitForFunction(
    async () => !(await navigator.serviceWorker.getRegistration()).waiting,
  );
  await page.waitForFunction(
    (text) => document.querySelector("textarea")?.value === text,
    draft,
  );
  laptopDisconnected = true;
  await reload();
  assert.equal(
    await page.locator('meta[name="studio-build"]').getAttribute("content"),
    nextBuild,
  );
  await page.waitForFunction(() => !document.querySelector(".studio-startup"), {
    timeout: 10000,
  });
  assert.equal(
    await page.evaluate(async () => {
      try {
        await fetch("/api/state");
        return "network returned data";
      } catch {
        return "offline";
      }
    }),
    "offline",
    "The worker cannot return cached API data as live data",
  );
  const cached = await page.evaluate(async () => {
    const names = (await caches.keys()).filter((name) =>
      name.startsWith("studio-shell-"),
    );
    return {
      names,
      urls: (
        await Promise.all(
          names.map(async (name) =>
            (await (await caches.open(name)).keys()).map(
              (request) => request.url,
            ),
          ),
        )
      ).flat(),
    };
  });
  assert.equal(cached.names.length, 2);
  assert.ok(
    cached.urls.every((url) => !new URL(url).pathname.startsWith("/api/")),
  );
  laptopDisconnected = false;
  await reload();
  await page.waitForFunction(
    (text) => document.querySelector("textarea")?.value === text,
    draft,
  );
  if (nextWorker) {
    await nextWorker.evaluate(() => {
      self.originalCacheOpen = caches.open.bind(caches);
      self.cacheFailures = 0;
      caches.open = async () => {
        self.cacheFailures++;
        throw new Error("Storage unavailable");
      };
    });
    await reload();
    await page.waitForFunction(
      (text) => document.querySelector("textarea")?.value === text,
      draft,
    );
    assert.ok(
      await nextWorker.evaluate(() => self.cacheFailures > 0),
      "The browser exercised unavailable cache storage",
    );
    await nextWorker.evaluate(() => {
      caches.open = self.originalCacheOpen;
    });
  }

  version = 3;
  await page.evaluate(async () => {
    const registration = await navigator.serviceWorker.getRegistration();
    await registration.update();
    const installing = registration.installing;
    if (!installing)
      throw new Error("Invalid update did not start installation");
    await new Promise((resolve, reject) => {
      const inspect = () => {
        if (installing.state === "redundant") resolve();
        if (installing.state === "installed")
          reject(new Error("Incomplete shell installed"));
      };
      installing.addEventListener("statechange", inspect);
      inspect();
    });
  });
  assert.deepEqual(
    await page.evaluate(() => caches.keys()),
    cached.names,
    "An incomplete update preserves both working build caches",
  );
  version = 2;
  assert.deepEqual(errors, []);
  await context.close();

  // An uncached script failure retains a clear, accessible reload action.
  rejectEntry = true;
  const broken = await browser.newPage({
    viewport: { width: 390, height: 844 },
  });
  await broken.goto(origin, { waitUntil: "domcontentloaded" });
  await broken.getByText("Studio could not open.", { exact: true }).waitFor();
  assert.ok(
    await broken.getByRole("link", { name: "Reload Studio" }).isVisible(),
  );
  await broken.close();
  console.log(
    JSON.stringify({
      result: "PASS",
      browser: useWebKit ? "WebKit" : "Chromium",
      throttled: !useWebKit,
      initial: bytes,
      initialTransferred: initialNetwork.reduce(
        (sum, request) => sum + request.bytes,
        0,
      ),
      offlineNetworkErrors: offlineNetworkErrors.length,
      coldMs,
      cachedSleepingMacMs: warmMs,
      cacheBuilds: cached.names.length,
      evidence: dir,
    }),
  );
} finally {
  await browser?.close();
  server?.closeAllConnections();
  await new Promise((resolve) => (server ? server.close(resolve) : resolve()));
  proc.kill("SIGTERM");
}
