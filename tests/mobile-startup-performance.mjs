// Production renderer and Runtime with generated history, no live user data.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { cp, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { gzipSync } from "node:zlib";
const repo = fileURLToPath(new URL("../", import.meta.url));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const dir = await mkdtemp(join(tmpdir(), "studio-mobile-performance-"));
await cp(join(repo, "web/dist"), join(dir, "dist"), { recursive: true });
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
  ["-B", join(repo, "tests/mobile-startup-fixture.py"), join(dir, "state")],
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
const measurements = {
  artifactBuild,
  network: {
    downloadBytesPerSecond: 200000,
    uploadBytesPerSecond: 100000,
    latencyMs: 100,
    cpuRate: 4,
  },
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
  const full = await (await fetch(origin + "/api/state")).text();
  const compact = await (await fetch(origin + "/api/state?view=chat")).text();
  Object.assign(measurements, {
    fixture,
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
    !JSON.parse(compact).runtime.work,
    "The chat snapshot excludes retained work history",
  );
  browser = await chromium.launch({
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
  page = await context.newPage();
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript((lead) => {
    localStorage.setItem("codex-mobile-opened", JSON.stringify(lead));
    const NativeEventSource = window.EventSource;
    window.performanceStreams = [];
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
  const requests = [];
  page.on("request", (request) =>
    requests.push({
      path: new URL(request.url()).pathname + new URL(request.url()).search,
      at: Date.now(),
      method: request.method(),
    }),
  );
  const cdp = await context.newCDPSession(page);
  await cdp.send("Network.enable");
  await cdp.send("Network.emulateNetworkConditions", {
    offline: false,
    latency: 100,
    downloadThroughput: 200000,
    uploadThroughput: 100000,
  });
  await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
  const started = Date.now();
  await page.goto(origin, { waitUntil: "domcontentloaded" });
  await page.locator("#message").waitFor({ timeout: 18000 });
  await page.waitForFunction(
    () => !document.querySelector("#message")?.disabled,
  );
  measurements.coldComposerMs = Date.now() - started;
  assert.ok(
    measurements.coldComposerMs < 10000,
    `Cold composer exceeds 10 seconds: ${measurements.coldComposerMs} ms`,
  );
  assert.equal(
    requests.filter((request) => request.path === "/api/state").length,
    0,
    "Startup does not download full state",
  );
  assert.ok(
    requests.some((request) => request.path.includes("scope=state%3Achat")),
    "Startup pulls the compact state projection",
  );
  assert.equal(
    requests.filter(
      (request) =>
        /^\/api\/sync\/pull\?/.test(request.path) &&
        new URL(request.path, origin).searchParams.get("scope") === "state",
    ).length,
    0,
    "Startup never pulls the full state projection",
  );
  await page.evaluate(() => navigator.serviceWorker.ready);
  const draft =
    "Retain this exact mobile performance draft after the app closes.";
  await page.locator("#message").fill(draft);
  await page.waitForFunction((text) => {
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index);
      if (
        key.startsWith("codex-drafts:") &&
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
  measurements.openSyncStreams = await page.evaluate(
    () =>
      window.performanceStreams.filter(
        (stream) =>
          stream.testOpen &&
          new URL(stream.url).pathname === "/api/sync/stream",
      ).length,
  );
  assert.equal(
    measurements.openSyncStreams,
    1,
    "The full app retains one shared sync stream",
  );
  assert.ok(
    idleRequests.filter((request) => request.path === "/api/session").length <=
      1,
    "Idle credentials do not poll every 1.6 seconds",
  );
  assert.ok(
    idleRequests.filter((request) => request.path.startsWith("/api/sync/pull?"))
      .length <= 12,
    "Idle projection polling remains bounded",
  );
  const warmStarted = Date.now();
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForFunction(
    (text) => document.querySelector("#message")?.value === text,
    draft,
    { timeout: 10000 },
  );
  measurements.warmDraftMs = Date.now() - warmStarted;
  assert.ok(
    measurements.warmDraftMs < 5000,
    `Cached chat and exact draft exceed 5 seconds: ${measurements.warmDraftMs} ms`,
  );
  assert.deepEqual(errors, []);
  measurements.requests = requests;
  await writeFile(
    join(dir, "result.json"),
    JSON.stringify(measurements, null, 2),
  );
  console.log(
    `PASS: mobile startup ${measurements.coldComposerMs} ms, cached draft ${measurements.warmDraftMs} ms, full state ${measurements.fullStateGzipBytes} bytes gzip, compact state ${measurements.compactStateGzipBytes} bytes gzip, one sync stream. Evidence: ${dir}`,
  );
} catch (error) {
  await page?.screenshot({ path: join(dir, "failure.png") });
  await writeFile(
    join(dir, "failure.json"),
    JSON.stringify({ measurements, errors, log }, null, 2),
  );
  console.error("Evidence:", dir);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
