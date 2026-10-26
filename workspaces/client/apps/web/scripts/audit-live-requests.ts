/** Read-only browser audit. Only the audited tab's sync subscription POST is allowed. */
import { chromium } from "playwright";
import type { Request } from "playwright";
import type { components } from "../src/generated/api.ts";
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
const { url: auditUrl, output, chat: chats } = values;
if (!auditUrl || !output || !chats || chats.length < 2)
  throw new Error(
    "Usage: audit-live-requests.ts --url URL --chat ID --chat ID --output PATH",
  );
const SCENARIO_DURATION_MS = 180_000;
const SWITCH_INTERVAL_MS = 20_000;
const WARMUP_MS = 8_000;
type RequestRow = {
  phase: string;
  url: string;
  method: string;
  at: string;
  subscriptions?: unknown;
  status?: number;
};
type AuditFrame = Partial<components["schemas"]["ResourceChangeEvent"]> & {
  gen?: unknown;
};
const report: {
  startedAt: string;
  url: string;
  chats: string[];
  scenarioDurationMs: number;
  switchIntervalMs: number;
  requests: RequestRow[];
  initiators: {
    phase: string;
    url: string;
    method: string;
    initiator: unknown;
    requestId: string;
    at: string;
  }[];
  fetchStacks: unknown[];
  scripts: { url: string; sourceMapURL: string | undefined }[];
  frames: {
    phase: string;
    event: string;
    at: string;
    reason: AuditFrame["reason"];
    resources: AuditFrame["resources"];
    revision: AuditFrame["revision"];
    gen: unknown;
    epoch: AuditFrame["epoch"];
    resourceVersions:
      | {
          revision: number;
          transcriptAfter: number | undefined;
          transcriptThrough: number | undefined;
          entityAfter: number | undefined;
          entityThrough: number | undefined;
        }[]
      | undefined;
  }[];
  errors: { phase: string; message: string }[];
  blocked: RequestRow[];
  switches: { chat: string; at: string }[];
  idleStartedAt?: string;
  switchingStartedAt?: string;
  idleEndedAt?: string;
  switchingEndedAt?: string;
  finishedAt?: string;
  counts?: Record<string, Record<string, number>>;
} = {
  startedAt: new Date().toISOString(),
  url: auditUrl,
  chats: chats,
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
  await mkdir(dirname(output), { recursive: true });
  await writeFile(output, JSON.stringify(report, null, 2), {
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
      url.origin === new URL(auditUrl).origin &&
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
    let frame: AuditFrame;
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
          url: String(args[0] instanceof Request ? args[0].url : args[0]),
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
  const requests = new Map<Request, RequestRow>();
  context.on("request", (request) => {
    const row: RequestRow = {
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
  await page.goto(auditUrl, {
    waitUntil: "domcontentloaded",
    timeout: 20_000,
  });
  const select = (chat: string) =>
    page.locator(`[data-chat="${chat}"]`).first().click({ timeout: 20_000 });
  const firstChat = chats[0];
  if (!firstChat) throw new Error("The audit needs a chat.");
  await select(firstChat);
  await page.waitForTimeout(WARMUP_MS);
  for (const scenario of ["idle", "switching"] as const) {
    phase = scenario;
    report[`${scenario}StartedAt`] = new Date().toISOString();
    console.log(JSON.stringify({ phase, at: report[`${scenario}StartedAt`] }));
    for (
      let index = 0;
      index < SCENARIO_DURATION_MS / SWITCH_INTERVAL_MS;
      index++
    ) {
      if (scenario === "switching") {
        const chat = chats[(index + 1) % chats.length];
        if (!chat) throw new Error("The audit needs a chat.");
        report.switches.push({ chat, at: new Date().toISOString() });
        await select(chat);
      }
      await page.waitForTimeout(SWITCH_INTERVAL_MS);
      await save();
    }
    report[`${scenario}EndedAt`] = new Date().toISOString();
  }
  report.finishedAt = new Date().toISOString();
  const counts: Record<string, Record<string, number>> = {};
  for (const request of report.requests) {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/api/")) continue;
    const key = `${request.method} ${url.pathname}`;
    const phases = (counts[key] ??= {});
    phases[request.phase] = (phases[request.phase] ?? 0) + 1;
  }
  report.counts = counts;
  console.log(
    JSON.stringify({
      output: output,
      counts,
      blocked: report.blocked.length,
    }),
  );
} finally {
  await save();
  await browser.close();
}
