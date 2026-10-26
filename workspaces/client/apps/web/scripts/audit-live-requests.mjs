/** Read-only browser audit. Only the audited tab's sync subscription POST is allowed. */
import { chromium } from "playwright";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    url: { type: "string" },
    chat: { type: "string", multiple: true },
    output: { type: "string" },
  },
});
if (!values.url || !values.output || (values.chat?.length ?? 0) < 2)
  throw new Error(
    "Usage: audit-live-requests.mjs --url URL --chat ID --chat ID --output PATH",
  );
const SCENARIO_DURATION_MS = 180_000;
const SWITCH_INTERVAL_MS = 20_000;
const WARMUP_MS = 8_000;
const report = {
  startedAt: new Date().toISOString(),
  url: values.url,
  chats: values.chat,
  scenarioDurationMs: SCENARIO_DURATION_MS,
  switchIntervalMs: SWITCH_INTERVAL_MS,
  requests: [],
  initiators: [],
  fetchStacks: [],
  scripts: [],
  frames: [],
  errors: [],
  blocked: [],
  switches: [],
};
let phase = "first-load";
const save = async () => {
  await mkdir(dirname(values.output), { recursive: true });
  await writeFile(values.output, JSON.stringify(report, null, 2), {
    mode: 0o600,
  });
};
const browser = await chromium.launch({ headless: true });
try {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 1000 },
    serviceWorkers: "block",
  });
  // Service-worker fetches must not bypass the write fence or request attribution.
  await context.route("**/*", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const subscription =
      request.method() === "POST" &&
      url.origin === new URL(values.url).origin &&
      url.pathname === "/api/sync/subscriptions";
    if (
      !["GET", "HEAD", "OPTIONS"].includes(request.method()) &&
      !subscription
    ) {
      report.blocked.push({
        phase,
        method: request.method(),
        url: request.url(),
        at: new Date().toISOString(),
      });
      await route.abort("blockedbyclient");
    } else await route.continue();
  });
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  await cdp.send("Network.enable");
  await cdp.send("Debugger.enable");
  cdp.on("Debugger.scriptParsed", (event) => {
    if (event.url)
      report.scripts.push({ url: event.url, sourceMapURL: event.sourceMapURL });
  });
  cdp.on("Network.requestWillBeSent", (event) =>
    report.initiators.push({
      phase,
      url: event.request.url,
      method: event.request.method,
      initiator: event.initiator,
      requestId: event.requestId,
      at: new Date().toISOString(),
    }),
  );
  cdp.on("Network.eventSourceMessageReceived", (event) => {
    // Keep resource and cursor metadata; omit transcript/provider text from summaries.
    let frame;
    try {
      frame = JSON.parse(event.data);
    } catch {
      return;
    }
    report.frames.push({
      phase,
      event: event.eventName,
      at: new Date().toISOString(),
      reason: frame.reason,
      resources: frame.resources,
      revision: frame.revision,
      gen: frame.gen,
      epoch: frame.epoch,
      resourceVersions: frame.resourceVersions?.map(
        ({ revision, transcriptChanges, entityChanges }) => ({
          revision,
          transcriptAfter: transcriptChanges?.after,
          transcriptThrough: transcriptChanges?.through,
          entityAfter: entityChanges?.after,
          entityThrough: entityChanges?.through,
        }),
      ),
    });
  });
  await page.addInitScript(() => {
    const original = window.fetch;
    window.fetch = function (...args) {
      console.debug(
        "AUDIT_INITIATOR",
        JSON.stringify({
          url: String(args[0]?.url ?? args[0]),
          stack: new Error().stack,
        }),
      );
      return original.apply(this, args);
    };
  });
  page.on("console", (message) => {
    if (message.text().startsWith("AUDIT_INITIATOR "))
      report.fetchStacks.push({
        phase,
        ...JSON.parse(message.text().slice("AUDIT_INITIATOR ".length)),
      });
    else if (message.type() === "error")
      report.errors.push({ phase, message: message.text().slice(0, 600) });
  });
  page.on("pageerror", (error) =>
    report.errors.push({ phase, message: error.message }),
  );
  const requests = new Map();
  context.on("request", (request) => {
    const row = {
      phase,
      url: request.url(),
      method: request.method(),
      at: new Date().toISOString(),
      ...(new URL(request.url()).pathname === "/api/sync/subscriptions"
        ? { subscriptions: request.postDataJSON() }
        : {}),
    };
    report.requests.push(row);
    requests.set(request, row);
  });
  context.on("response", (response) => {
    const row = requests.get(response.request());
    if (row) row.status = response.status();
  });
  await page.goto(values.url, {
    waitUntil: "domcontentloaded",
    timeout: 20_000,
  });
  const select = (chat) =>
    page.locator(`[data-chat="${chat}"]`).first().click({ timeout: 20_000 });
  await select(values.chat[0]);
  await page.waitForTimeout(WARMUP_MS);
  for (const scenario of ["idle", "switching"]) {
    phase = scenario;
    report[`${scenario}StartedAt`] = new Date().toISOString();
    console.log(JSON.stringify({ phase, at: report[`${scenario}StartedAt`] }));
    for (
      let index = 0;
      index < SCENARIO_DURATION_MS / SWITCH_INTERVAL_MS;
      index++
    ) {
      if (scenario === "switching") {
        const chat = values.chat[(index + 1) % values.chat.length];
        report.switches.push({ chat, at: new Date().toISOString() });
        await select(chat);
      }
      await page.waitForTimeout(SWITCH_INTERVAL_MS);
      await save();
    }
    report[`${scenario}EndedAt`] = new Date().toISOString();
  }
  report.finishedAt = new Date().toISOString();
  const counts = {};
  for (const request of report.requests) {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/api/")) continue;
    const key = `${request.method} ${url.pathname}`;
    counts[key] ??= {};
    counts[key][request.phase] = (counts[key][request.phase] ?? 0) + 1;
  }
  report.counts = counts;
  console.log(
    JSON.stringify({
      output: values.output,
      counts,
      blocked: report.blocked.length,
    }),
  );
} finally {
  await save();
  await browser.close();
}
