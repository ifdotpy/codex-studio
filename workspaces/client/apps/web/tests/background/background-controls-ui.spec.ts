import { required } from "../teams/helpers.ts";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import type { Agent, Monitor, BackgroundTask } from "../../src/types.ts";
import type { PostBody, PostResult, GetResult } from "../../src/apiTypes.ts";
type FixtureSync = Parameters<typeof handleEntitySyncFixtureRequest>[2];
type RecordedBody = Partial<
  PostBody<"/api/processes/action"> & PostBody<"/api/monitor/input">
>;
declare global {
  interface Window {
    backgroundControlsSchemaHandshakeEvents: string[];
  }
}
import {
  handleEntitySyncFixtureRequest,
  readFixtureSyncContract,
  readApiSchemaHash,
  syncIdentityFixture,
  stubEntityState,
  expect,
  test,
  updateEntitySyncFixture,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, extname } from "node:path";
test("Background controls", async ({ context }, testInfo) => {
  test.setTimeout(180000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Browser contract for process controls. The HTTP transport is a deterministic fixture.
  const skill = testRepo;
  const root = await mkdtemp(join(tmpdir(), "codex-background-controls-"));
  let syncFixture: FixtureSync | undefined;
  const streamListeners = new Set<
    Parameters<NonNullable<FixtureSync["onStreamReady"]>>[0]
  >();
  const server = createServer(async (req, res) => {
    try {
      const url = new URL(req.url || "/", "http://localhost");
      if (req.method === "POST" && url.pathname === "/api/monitor/cancel") {
        for await (const _chunk of req) {
          // The fixture action has no body fields beyond its id.
        }
        monitor.cancelRequested = true;
        monitor.error = "Stop requested; waiting for the command to exit";
        const syncEntities = updateEntitySyncFixture(state);
        res.setHeader("Content-Type", "application/json");
        res.end(JSON.stringify({ ...monitor, _syncEntities: syncEntities }));
        return;
      }
      if (req.method === "POST" && url.pathname === "/api/sync/notify") {
        let body = "";
        for await (const chunk of req) body += chunk;
        // This fixture reads the notification written by the renderer generated client.
        const notification: PostBody<"/api/sync/notify"> = JSON.parse(body);
        for (const publish of streamListeners) publish([{ kind: "state" }]);
        res.setHeader("Content-Type", "application/json");
        res.end(
          JSON.stringify({ accepted: true, requestId: notification.requestId }),
        );
        return;
      }
      if (syncFixture && handleEntitySyncFixtureRequest(req, res, syncFixture))
        return;
      const name = new URL(req.url || "/", "http://localhost").pathname;
      const path = join(
        skill,
        "workspaces/client/apps/web/dist",
        name === "/" ? "index.html" : name,
      );
      res.setHeader(
        "Content-Type",
        { ".js": "text/javascript", ".css": "text/css", ".html": "text/html" }[
          extname(path)
        ] || "application/octet-stream",
      );
      res.end(await readFile(path));
    } catch {
      res.statusCode = 404;
      res.end();
    }
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const agent: Agent = {
    id: "lead",
    rootId: "lead",
    isLead: true,
    source: "managed",
    status: "idle",
    inFlight: true,
    accountKey: "default",
    epoch: 4,
    threadId: "thread-fixture",
    turnId: "turn-fixture",
    name: "Release lead",
    model: "gpt-5.6-sol",
    created: Date.now() / 1000 - 30,
    canSend: true,
  };
  const native: BackgroundTask = {
    id: "native-task",
    agent: "lead",
    kind: "command",
    name: "commandExecution",
    command: "native test process",
    processId: "9042",
    status: "running",
    created: Date.now() / 1000 - 20,
    tail: "Waiting for input",
  };
  const responseTarget: NonNullable<
    GetResult<"/api/processes">["responseTarget"]
  > = {
    agent: "lead",
    accountKey: "default",
    epoch: 4,
    connectionId: "connection-fixture",
    threadId: "thread-fixture",
    turnId: "turn-fixture",
  };
  const claudeTask = {
    id: "lead:task:background-1",
    agent: "lead",
    accountKey: "default",
    epoch: 4,
    source: "claude_task",
    status: "running",
    created: Date.now() / 1000 - 10,
    name: "Claude background task",
    capabilities: {
      readOutput: false,
      sendInput: false,
      resize: false,
      stop: true,
    },
  };
  const monitor: Monitor & Pick<BackgroundTask, "kind"> = {
    id: "6f2ed4dd-f6ab-4d46-94a5-8a774333876a",
    agent: "lead",
    kind: "monitor",
    command: "read value; echo $value",
    interactive: true,
    timeout_ms: 1800000,
    status: "running",
    created: Date.now() / 1000,
    tail: "Monitor output text",
    error: "Monitor error line",
    log: "/fixture/monitor.log",
  };
  const state = {
    ...fixtureSnapshot([agent]),
    token: "fixture-token",
    stateDir: root,
    threads: [agent],
    chats: [],
    runtime: {
      ...fixtureSnapshot([agent]).runtime,
      nativeNotices: [],
      sidebarOrder: { groups: null, revision: 0 },
      rateLimits: null,
      rateLimitsByAccount: {},
      agents: [agent],
      rooms: [],
      complaints: [],
      monitors: [monitor],
      tasks: [native],
      requests: [],
    },
  };
  const workspaceId = syncIdentityFixture().workspaceId;
  updateEntitySyncFixture(state);
  syncFixture = {
    snapshot: state,
    workspaceId,
    onStreamReady: (publish) => {
      streamListeners.add(publish);
    },
  };
  const writes: { path: string; body: RecordedBody }[] = [];
  const processReceipts = new Map<
    string,
    PostResult<"/api/processes/action">
  >();
  let selectedStopDispatches = 0;
  let dropNextSelectedStopReply = false;
  let rejectNextResponseStop = false;
  let emptyProcessList = false;
  let rejectNextReceiptLookup = false;
  let receiptLookupCount = 0;
  let failInput = false;
  let page;
  try {
    page = await context.newPage();
    await page.setViewportSize({ width: 1440, height: 980 });
    await page.addInitScript(() => {
      window.backgroundControlsSchemaHandshakeEvents = [];
      window.EventSource = class extends EventSource {
        constructor(url: string | URL, config?: EventSourceInit) {
          super(url, config);
          this.addEventListener("api-schema", (event) =>
            window.backgroundControlsSchemaHandshakeEvents.push(
              String(event.data),
            ),
          );
        }
      };
    });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.route("**/api/**", async (route) => {
      const req = route.request(),
        path = new URL(req.url()).pathname;
      let body: RecordedBody = {};
      if (req.method() === "POST") {
        body = req.postDataJSON();
        assert.equal(req.headers()["x-canvas-token"], "fixture-token", path);
        writes.push({ path, body });
      }
      let value: unknown = {};
      if (path.startsWith("/api/sync/")) return route.continue();
      if (path === "/api/session") value = { token: "fixture-token" };
      else if (path === "/api/accounts")
        value = { accounts: [], defaultAccountKey: "default" };
      else if (path === "/api/voice/records")
        value = { records: [], delivered: [], cursor: 0 };
      else if (path === "/api/workspace/tasks")
        value = {
          tasks: [native],
          cursor: { updated: native.created, id: native.id },
          hasMore: false,
          hasMoreChanges: false,
          reset: false,
        };
      else if (path === "/api/workspace")
        value = { tasks: [native], monitors: [monitor] };
      else if (path === "/api/processes" && req.method() === "GET")
        value = {
          responseTarget,
          processes: emptyProcessList
            ? []
            : [
                {
                  id: monitor.id,
                  key: "monitor-key",
                  generation: "monitor-generation",
                  agent: "lead",
                  accountKey: "default",
                  epoch: 4,
                  source: "monitor",
                  status: "running",
                  cancelRequested: !!monitor.cancelRequested,
                  created: monitor.created,
                  name: monitor.command,
                  command: monitor.command,
                  output: monitor.tail,
                  error: monitor.error,
                  log: monitor.log,
                  capabilities: {
                    readOutput: true,
                    sendInput: true,
                    resize: true,
                    stop: true,
                  },
                },
                {
                  ...claudeTask,
                  key: "claude-task-key",
                  generation: "claude-task-generation",
                  output: "",
                },
                {
                  id: native.id,
                  key: "native-key",
                  generation: "native-generation",
                  agent: "lead",
                  accountKey: "default",
                  epoch: 4,
                  source: "codex_task",
                  status: "running",
                  cancelRequested: false,
                  created: native.created,
                  name: native.name,
                  command: native.command,
                  output: native.tail,
                  capabilities: {
                    readOutput: true,
                    sendInput: true,
                    resize: false,
                    stop: true,
                  },
                },
              ],
        };
      else if (path === "/api/processes/action") {
        if (req.method() === "GET") {
          receiptLookupCount++;
          if (rejectNextReceiptLookup) {
            rejectNextReceiptLookup = false;
            return route.fulfill({
              status: 400,
              json: { error: "Receipt lookup query rejected" },
            });
          }
          const requestId = new URL(req.url()).searchParams.get("requestId");
          const result = processReceipts.get(required(requestId));
          return route.fulfill({
            json: { found: !!result, result: result || null },
          });
        }
        if (body.action === "stop_response" && rejectNextResponseStop) {
          rejectNextResponseStop = false;
          return route.fulfill({
            status: 400,
            json: { error: "Response changed. Refresh the process list" },
          });
        }
        value = processReceipts.get(required(body.requestId));
        if (!value) {
          if (body.action === "stop_selected") {
            selectedStopDispatches++;
            monitor.cancelRequested = true;
          }
          const result: PostResult<"/api/processes/action"> = {
            requestId: required(body.requestId),
            status: "completed",
            processes:
              body.action === "stop_selected"
                ? [
                    {
                      key: required(body.process).key,
                      agent: required(body.process).agent,
                      status: "requested",
                    },
                  ]
                : [],
          };
          value = result;
          processReceipts.set(required(body.requestId), result);
          if (body.action === "stop_selected" && dropNextSelectedStopReply) {
            dropNextSelectedStopReply = false;
            return route.abort();
          }
        }
      } else if (path === "/api/task") value = native;
      else if (path === "/api/transcript") value = { items: [], agent };
      else if (path === "/api/transcript/stream")
        return route.fulfill({ status: 503, body: "fixture polling" });
      else if (path === "/api/limits") value = { data: null };
      else if (path === "/api/monitor")
        return route.fulfill({ status: 404, json: { error: "Not found" } });
      else if (path === "/api/monitor/log")
        return route.fulfill({
          status: 200,
          body: Buffer.from("full saved log\n"),
          headers: {
            "Content-Type": "text/plain",
            "Content-Disposition": 'attachment; filename="monitor.log"',
            "X-Log-Truncated": "true",
          },
        });
      else if (path === "/api/multi-server")
        value = {
          protocol: 1,
          identity: {
            serverId: "fixture-server",
            label: "Studio server",
            origin: null,
            publicKey:
              "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA03Gf93BzL6we9QAue5E1GRzfNBfEMRLv97mjj9pUqPU=\n-----END PUBLIC KEY-----\n",
            tailscaleUser: null,
          },
          clients: [],
          servers: [],
          invites: [],
          settings: { autoPair: true },
        };
      else if (path === "/api/monitor/cancel") {
        return route.continue();
      } else if (path === "/api/monitor/input" && failInput) {
        failInput = false;
        return route.fulfill({
          status: 409,
          json: { error: "The command already exited" },
        });
      } else if (req.method() === "GET") {
        return route.fulfill({
          status: 404,
          json: { error: "Not part of this fixture" },
        });
      }
      await route.fulfill({ json: value });
    });
    const address = server.address();
    if (!address || typeof address === "string")
      throw new Error("Missing fixture TCP address");
    const origin = `http://127.0.0.1:${address.port}`;
    const entityState = await stubEntityState(
      page,
      state,
      await readFixtureSyncContract(origin),
    );
    await page.goto(origin, { waitUntil: "domcontentloaded", timeout: 15000 });
    await page.waitForFunction(
      () => window.backgroundControlsSchemaHandshakeEvents?.length > 0,
      undefined,
      { timeout: 5000 },
    );
    const handshakes = await page.evaluate(() =>
      window.backgroundControlsSchemaHandshakeEvents.map((data) =>
        JSON.parse(data),
      ),
    );
    assert.equal(handshakes[0].hash, readApiSchemaHash());
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator("#tasks-toggle").click();
    const drawer = page.getByRole("dialog", { name: /Current activity/ });
    assert.equal(
      await page
        .getByRole("alertdialog", { name: "Studio update required" })
        .isVisible(),
      false,
      "the fixture identity and stream use the generated schema",
    );
    rejectNextResponseStop = true;
    await drawer
      .getByRole("button", { name: "Stop response", exact: true })
      .click();
    assert.equal(required(writes.at(-1)).path, "/api/processes/action");
    assert.equal(required(writes.at(-1)).body.action, "stop_response");
    assert.deepEqual(
      required(writes.at(-1)).body.responseTarget,
      responseTarget,
    );
    await page
      .getByText("Response changed. Refresh the process list", { exact: true })
      .waitFor();
    await drawer
      .getByRole("button", { name: "Stop response", exact: true })
      .click();
    assert.deepEqual(
      required(writes.at(-1)).body.responseTarget,
      responseTarget,
    );
    assert.equal(
      monitor.status,
      "running",
      "response stop leaves the monitor running",
    );
    const unchangedMonitors = state.runtime.monitors;
    const unchangedTasks = state.runtime.tasks;
    Object.assign(agent, { inFlight: false, turnId: null, status: "idle" });
    await entityState.update(state, { origin, token: "fixture-token" });
    await expect(
      drawer.getByRole("button", { name: "Stop response", exact: true }),
    ).toBeDisabled();
    Object.assign(agent, {
      inFlight: true,
      turnId: "turn-next",
      status: "running",
    });
    responseTarget.turnId = "turn-next";
    await entityState.update(state, { origin, token: "fixture-token" });
    await drawer
      .getByRole("button", { name: "Stop response", exact: true })
      .waitFor({ state: "visible" });
    await expect(
      drawer.getByRole("button", { name: "Stop response", exact: true }),
    ).toBeEnabled();
    await drawer
      .getByRole("button", { name: "Stop response", exact: true })
      .click();
    assert.deepEqual(
      required(writes.at(-1)).body.responseTarget,
      responseTarget,
    );
    assert.equal(state.runtime.monitors, unchangedMonitors);
    assert.equal(state.runtime.tasks, unchangedTasks);
    await drawer
      .getByRole("button", { name: "Stop owned processes", exact: true })
      .click();
    assert.equal(required(writes.at(-1)).body.action, "stop_owned");
    assert.equal(required(writes.at(-1)).body.includeDescendants, false);
    assert.deepEqual(
      required(required(writes.at(-1)).body.processes).map(
        (process) => process.key,
      ),
      ["monitor-key", "claude-task-key", "native-key"],
    );
    await drawer
      .getByRole("heading", { name: "Background tasks", exact: true })
      .waitFor();
    await drawer.locator('[data-task="lead:task:background-1"]').waitFor();
    assert.equal(
      await drawer
        .getByRole("button", { name: /^(New monitor|Start monitor)$/ })
        .count(),
      0,
    );
    await drawer.locator(`[data-task="${monitor.id}"]`).click();
    assert.match(
      await drawer.locator(".task-output").innerText(),
      /Monitor output text/,
    );
    assert.ok(
      (await drawer
        .getByRole("alert")
        .filter({ hasText: "Monitor error line" })
        .count()) >= 1,
    );
    await drawer
      .getByRole("button", { name: "Send line", exact: true })
      .waitFor();
    assert.equal(
      writes.some((w) => w.path === "/api/monitor"),
      false,
      "opening an agent monitor does not start a command",
    );
    const created = monitor;
    const input = drawer.getByLabel("Terminal input");
    await input.fill("hello");
    await drawer
      .getByRole("button", { name: "Send line", exact: true })
      .click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLTextAreaElement>(
          '[aria-label="Terminal input"]',
        )?.value === "",
    );
    assert.deepEqual(writes.at(-1), {
      path: "/api/monitor/input",
      body: { id: created.id, text: "hello\n" },
    });
    failInput = true;
    await input.fill("keep this input");
    await drawer
      .getByRole("button", { name: "Send line", exact: true })
      .click();
    await page
      .getByText("The command already exited", { exact: true })
      .waitFor();
    assert.equal(await input.inputValue(), "keep this input");
    await drawer.getByRole("button", { name: "Ctrl+C", exact: true }).click();
    assert.equal(required(writes.at(-1)).body.text, "\u0003");
    await drawer.locator(".process-resize summary").click();
    await drawer.getByLabel("Rows", { exact: true }).fill("30");
    await drawer.getByLabel("Columns", { exact: true }).fill("100");
    await drawer.getByRole("button", { name: "Resize", exact: true }).click();
    assert.deepEqual(required(writes.at(-1)).body, {
      id: created.id,
      rows: 30,
      cols: 100,
    });
    const downloaded = page.waitForEvent("download");
    await drawer.getByLabel("Download task log").click();
    const download = await downloaded;
    assert.equal(download.suggestedFilename(), "monitor.log");
    assert.equal(
      await readFile(await download.path(), "utf8"),
      "full saved log\n",
    );
    await page
      .getByText("The download contains the retained part of the log.", {
        exact: true,
      })
      .waitFor();
    // The mobile client closes desktop drawers below 761px. Exercise this drawer
    // at a narrow desktop width; mobile has its own client tests.
    await page.setViewportSize({ width: 780, height: 740 });
    assert.equal(
      await page.evaluate(() => document.documentElement.scrollWidth),
      780,
    );
    await drawer.locator(".process-input").scrollIntoViewIfNeeded();
    await drawer
      .getByRole("button", { name: "Close input (EOF)", exact: true })
      .click();
    await drawer
      .getByRole("button", { name: "Input closed", exact: true })
      .waitFor();
    assert.equal(required(writes.at(-1)).body.closeStdin, true);
    assert.equal(await input.isDisabled(), true);
    dropNextSelectedStopReply = true;
    await drawer
      .getByRole("button", { name: "Stop selected process", exact: true })
      .click();
    assert.equal(required(writes.at(-1)).path, "/api/processes/action");
    assert.equal(required(writes.at(-1)).body.action, "stop_selected");
    assert.equal(required(writes.at(-1)).body.process?.key, "monitor-key");
    assert.equal(
      monitor.status,
      "running",
      "The stop request does not claim that the process exited",
    );
    const lostReply = required(writes.at(-1)).body;
    await page
      .getByText(
        "The stop reply is unknown. Check its receipt before another stop.",
      )
      .waitFor();
    await page.reload();
    await page.getByRole("heading", { name: /Release lead/ }).waitFor();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator("#tasks-toggle").click();
    rejectNextReceiptLookup = true;
    await drawer
      .getByRole("button", { name: "Check stop result", exact: true })
      .click();
    await page
      .getByText(
        "The stop result is still unknown. Check its receipt again later.",
      )
      .waitFor();
    assert.equal(receiptLookupCount, 1);
    assert.equal(selectedStopDispatches, 1);
    assert.equal(
      writes.filter((write) => write.body.requestId === lostReply.requestId)
        .length,
      1,
    );
    await drawer
      .getByRole("button", { name: "Check stop result", exact: true })
      .click();
    await page.getByText("Stop result: requested", { exact: true }).waitFor();
    assert.equal(receiptLookupCount, 2);
    assert.equal(selectedStopDispatches, 1);
    await drawer.locator('[data-task="native-task"]').click();
    await drawer
      .getByRole("button", { name: "Send via agent", exact: true })
      .waitFor();
    await drawer.getByLabel("Terminal input").fill("continue");
    await drawer
      .getByRole("button", { name: "Send via agent", exact: true })
      .click();
    assert.deepEqual(writes.at(-1), {
      path: "/api/native-command",
      body: { id: "native-task", text: "continue\n", action: "input" },
    });
    await drawer
      .getByRole("button", { name: "Stop selected process", exact: true })
      .click();
    assert.equal(required(writes.at(-1)).path, "/api/processes/action");
    assert.equal(required(writes.at(-1)).body.action, "stop_selected");
    assert.equal(required(writes.at(-1)).body.process?.key, "native-key");
    await page.screenshot({
      path: testInfo.outputPath("native-command-controls.png"),
    });
    assert.equal(
      writes.some(
        (w) => w.path === "/api/monitor/input" && w.body.id === "9042",
      ),
      false,
    );
    emptyProcessList = true;
    await page.reload();
    await page.getByRole("heading", { name: /Release lead/ }).waitFor();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator("#tasks-toggle").click();
    await expect(
      page
        .getByRole("dialog", { name: /Current activity/ })
        .getByRole("button", { name: "Stop owned processes", exact: true }),
    ).toBeDisabled();
    agent.status = "starting";
    agent.startAttempt = {
      prepareError: "Thread preparation acknowledgement pending",
    };
    agent.error = "Thread preparation acknowledgement pending";
    const failedWorker: Agent = {
      ...agent,
      id: "pending-review",
      parentId: agent.id,
      isLead: false,
      name: "pending-review",
      role: "reviewer",
      status: "failed",
      error: "thread/resume response timed out; outcome unknown",
    };
    state.threads.push(failedWorker);
    state.runtime.agents.push(failedWorker);
    await page.setViewportSize({ width: 1440, height: 980 });
    await page.reload();
    await page
      .locator("#conversation-title")
      .getByText("Release lead", { exact: true })
      .waitFor();
    await page.locator('.agent-phase[data-phase="acknowledgement"]').waitFor();
    await page.getByRole("button", { name: "Team", exact: true }).click();
    await page
      .locator('[data-worker="pending-review"] .worker-error')
      .waitFor();
    assert.equal(
      await page
        .locator('[data-worker="pending-review"] .worker-error')
        .innerText(),
      failedWorker.error,
    );
    assert.deepEqual(errors, []);
    console.log(`PASS background controls browser contract. Evidence: ${root}`);
  } finally {
    server.closeAllConnections();
    await page?.close();
    await new Promise<void>((r) => server.close(() => r()));
  }
});
