import {
  access,
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  unlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  test,
  expect,
  spawnFixture,
} from "../../../../tests/client/playwright.mjs";

test("an active AgentPanel follows native progress changes without polling", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const repo = join(import.meta.dirname, "../../../..");
  const builtIndex = await readFile(join(repo, "web/dist/index.html"), "utf8");
  const builtEntry = builtIndex.match(/src="\.\/(assets\/index-[^"]+\.js)"/);
  expect(builtEntry, "built entrypoint in web/dist/index.html").not.toBeNull();
  const expectedEntry = builtEntry[1];
  const root = await mkdtemp(join(tmpdir(), "studio-agent-panel-push-"));
  const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map(
    (model) => ({
      model,
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: ["low", "medium", "high"].map(
        (reasoningEffort) => ({ reasoningEffort }),
      ),
      serviceTiers: [{ id: "priority" }],
    }),
  );
  const proc = spawnFixture(
    process.env.PYTHON || "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
        CODEX_BOARD_STATE_DIR: join(root, "board"),
        PROGRESS_PUSH_UI_FIXTURE: "1",
      },
    },
  );
  let context;
  let fixtureLog = "";
  proc.stderr.on("data", (data) => {
    fixtureLog += data;
  });

  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error("Fixture startup timed out: " + fixtureLog)),
      30_000,
    );
    proc.stdout.once("data", (data) => {
      clearTimeout(timer);
      resolve(Number(String(data).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timer);
      reject(Error(fixtureLog || "Fixture exited"));
    });
  });
  const origin = `http://127.0.0.1:${port}`;

  try {
    const initial = await (await fetch(origin + "/api/state")).json();
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const other = initial.threads.find(
      (agent) => agent.name === "Other project",
    );
    expect(other?.isLead).toBe(true);
    const panelResponse = await fetch(`${origin}/api/panel?agent=${lead.id}`);
    const panel = await panelResponse.json();
    expect(panel.path.startsWith(root + "/")).toBe(true);
    await mkdir(dirname(panel.path), { recursive: true });
    await writeFile(panel.path, "Initial active progress.");
    const otherPanelResponse = await fetch(
      `${origin}/api/panel?agent=${other.id}`,
    );
    expect(otherPanelResponse.status).toBe(200);
    const otherPanel = await otherPanelResponse.json();
    expect(otherPanel.agent).toBe(other.id);
    expect(otherPanel.error).toBeNull();
    expect(otherPanel.path.startsWith(root + "/")).toBe(true);
    await expect(access(dirname(otherPanel.path))).rejects.toMatchObject({
      code: "ENOENT",
    });
    await writeFile(
      join(dirname(panel.path), ".PROGRESS.md-replacement"),
      "Atomically replaced progress.",
    );
    const replacement = join(dirname(panel.path), ".PROGRESS.md-replacement");

    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
      serviceWorkers: "block",
    });
    await context.addInitScript(
      ({ id, stateDir }) => {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
        window.__broadcastReceived = [];
        window.__broadcastSent = [];
        window.__broadcastTimeline = [];
        window.__broadcastPostErrors = [];
        window.__syncChannels = [];
        const captureBroadcast = (direction, message) => {
          const entry = {
            direction,
            message,
            streams: (window.__resourceEventSources || []).map((source) => ({
              opened: source.__opened,
              readyState: source.readyState,
              closedByClient: Boolean(source.__closedByClient),
            })),
          };
          window.__broadcastTimeline.push(entry);
          const panelRef = (resource) =>
            resource.kind === "panel" && resource.agentId === id;
          const relevant =
            message.kind === "discover-subscriptions" ||
            (message.kind === "subscriptions" &&
              message.resources.some(panelRef)) ||
            (message.kind === "resource-event" &&
              message.event.resources.some(panelRef));
          if (relevant && navigator.locks?.query) {
            void navigator.locks
              .query()
              .then(({ held, pending }) => {
                entry.locks = {
                  held: held.map(({ name, mode }) => ({ name, mode })),
                  pending: pending.map(({ name, mode }) => ({ name, mode })),
                };
              })
              .catch((error) => {
                entry.lockQueryError = String(error);
              });
          }
        };
        const OriginalBroadcastChannel = window.BroadcastChannel;
        if (OriginalBroadcastChannel) {
          window.BroadcastChannel = function (name) {
            const channel = new OriginalBroadcastChannel(name);
            const post = channel.postMessage.bind(channel);
            const close = channel.close.bind(channel);
            const isSyncChannel = String(name).startsWith("codex-sync-");
            const info = { name: String(name), closed: false };
            if (isSyncChannel) {
              window.__syncChannels.push(info);
            }
            channel.postMessage = (message) => {
              if (isSyncChannel) {
                window.__broadcastSent.push(message);
                captureBroadcast("send", message);
              }
              try {
                post(message);
              } catch (error) {
                if (isSyncChannel)
                  window.__broadcastPostErrors.push(String(error));
                throw error;
              }
            };
            channel.close = () => {
              info.closed = true;
              close();
            };
            Object.defineProperty(channel, "onmessage", {
              configurable: true,
              set(handler) {
                if (typeof handler !== "function") return;
                channel.addEventListener("message", (event) => {
                  if (isSyncChannel)
                    window.__broadcastReceived.push(event.data);
                  if (isSyncChannel) captureBroadcast("receive", event.data);
                  handler.call(channel, event);
                });
              },
            });
            return channel;
          };
          window.BroadcastChannel.prototype =
            OriginalBroadcastChannel.prototype;
          Object.setPrototypeOf(
            window.BroadcastChannel,
            OriginalBroadcastChannel,
          );
        }

        const OriginalEventSource = window.EventSource;
        const sources = [];
        function TrackingEventSource(url, options) {
          const source = new OriginalEventSource(url, options);
          source.__trackedUrl = String(url);
          source.__opened = false;
          source.__errorCount = 0;
          source.__resourceEvents = [];
          source.addEventListener("open", () => {
            source.__opened = true;
          });
          source.addEventListener("error", () => {
            source.__errorCount++;
          });
          source.addEventListener("resources", (event) => {
            source.__resourceEvents.push(JSON.parse(event.data));
          });
          const close = source.close.bind(source);
          source.close = () => {
            source.__closedByClient = true;
            close();
          };
          sources.push(source);
          return source;
        }
        TrackingEventSource.prototype = OriginalEventSource.prototype;
        Object.setPrototypeOf(TrackingEventSource, OriginalEventSource);
        window.EventSource = TrackingEventSource;
        window.__resourceEventSources = sources;
      },
      { id: lead.id, stateDir: initial.stateDir },
    );

    const page = await context.newPage();
    const panelGets = [];
    const layoutPosts = [];
    const streamResponses = [];
    const servedAssets = [];
    const statusGets = [];
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (url.pathname === "/api/panel" && request.method() === "GET")
        panelGets.push(url.searchParams.get("agent"));
      if (url.pathname === "/api/panel/layout" && request.method() === "POST")
        layoutPosts.push(request);
      if (
        request.method() === "GET" &&
        ["/api/status", "/api/sync/status", "/api/sync/heartbeat"].includes(
          url.pathname,
        )
      )
        statusGets.push(url.pathname);
    });
    page.on("response", (response) => {
      const url = new URL(response.url());
      if (url.pathname === "/api/sync/stream")
        streamResponses.push({ url: url.href, status: response.status() });
      if (url.pathname.startsWith("/assets/"))
        servedAssets.push({
          path: url.pathname.slice(1),
          status: response.status(),
        });
    });
    await page.goto(origin);
    await expect
      .poll(() =>
        servedAssets.some(
          (asset) => asset.path === expectedEntry && asset.status === 200,
        ),
      )
      .toBe(true);
    await page.locator("#message").waitFor();
    const progress = () =>
      page.getByRole("region", { name: "Agent progress", exact: true });
    const current = () =>
      progress().locator(".agent-panel-current, .agent-panel-saved");
    await current()
      .getByText("Initial active progress.", { exact: true })
      .waitFor();
    await page.waitForFunction((agentId) => {
      const entries = window.__resourceEventSources || [];
      return entries.some((source) => {
        const resources = JSON.parse(
          new URL(source.url, location.href).searchParams.get("resources") ||
            "[]",
        );
        return resources.some(
          (resource) =>
            resource.kind === "panel" && resource.agentId === agentId,
        );
      });
    }, lead.id);
    const watchState = async () =>
      (await fetch(`${origin}/api/test/progress-watch-state`)).json();
    await expect.poll(watchState).toMatchObject({
      activeAgentIds: expect.arrayContaining([lead.id]),
      dispatcherAlive: true,
      observerAlive: true,
    });

    const follower = await context.newPage();
    const followerPanelGets = [];
    follower.on("request", (request) => {
      const url = new URL(request.url());
      if (url.pathname === "/api/panel" && request.method() === "GET")
        followerPanelGets.push(url.searchParams.get("agent"));
    });
    await follower.goto(origin);
    await expect
      .poll(() =>
        servedAssets.some(
          (asset) => asset.path === expectedEntry && asset.status === 200,
        ),
      )
      .toBe(true);
    await follower.locator("#message").waitFor();
    const followerProgress = () =>
      follower.getByRole("region", { name: "Agent progress", exact: true });
    const followerCurrent = () =>
      followerProgress().locator(".agent-panel-current, .agent-panel-saved");
    await followerCurrent()
      .getByText("Initial active progress.", { exact: true })
      .waitFor();
    await follower.waitForFunction(
      (agentId) =>
        (window.__broadcastSent || []).some(
          (message) =>
            message.kind === "subscriptions" &&
            message.resources.some(
              (resource) =>
                resource.kind === "panel" && resource.agentId === agentId,
            ),
        ),
      lead.id,
    );
    try {
      await page.waitForFunction(
        (agentId) => {
          const timeline = window.__broadcastTimeline || [];
          const followerSubscribeIndex = timeline.findIndex(
            ({ direction, message }) =>
              direction === "receive" &&
              message.kind === "subscriptions" &&
              message.resources.some(
                (resource) =>
                  resource.kind === "panel" && resource.agentId === agentId,
              ),
          );
          return (
            followerSubscribeIndex >= 0 &&
            timeline
              .slice(followerSubscribeIndex + 1)
              .some(
                ({ direction, message }) =>
                  direction === "send" &&
                  message.kind === "resource-event" &&
                  message.event.resources.some(
                    (resource) =>
                      resource.kind === "panel" && resource.agentId === agentId,
                  ),
              )
          );
        },
        lead.id,
        { timeout: 5_000 },
      );
    } catch (error) {
      const ownerTimeline = await page.evaluate(
        (agentId) =>
          (window.__broadcastTimeline || [])
            .filter(({ message }) => {
              if (message.kind === "discover-subscriptions") return true;
              if (message.kind === "subscriptions")
                return message.resources.some(
                  (resource) =>
                    resource.kind === "panel" && resource.agentId === agentId,
                );
              return (
                message.kind === "resource-event" &&
                message.event.resources.some(
                  (resource) =>
                    resource.kind === "panel" && resource.agentId === agentId,
                )
              );
            })
            .map(({ direction, message, streams, locks, lockQueryError }) => ({
              direction,
              kind: message.kind,
              tabId: message.tabId,
              reset: message.reset,
              reason: message.event?.reason,
              revision: message.event?.revision,
              resources: message.resources || message.event?.resources,
              streams,
              locks,
              lockQueryError,
            })),
        lead.id,
      );
      throw new Error(
        `${error.message}\nOwner resource timeline: ${JSON.stringify(ownerTimeline)}`,
      );
    }
    await follower.waitForFunction(
      (agentId) =>
        (window.__broadcastReceived || []).some(
          (message) =>
            message.kind === "resource-event" &&
            message.event.resources.some(
              (resource) =>
                resource.kind === "panel" && resource.agentId === agentId,
            ),
        ),
      lead.id,
      { timeout: 5_000 },
    );
    const followerDiagnostics = await follower.evaluate(() => {
      const describe = (message) => ({
        kind: message.kind,
        tabId: message.tabId,
        workspaceId: message.workspaceId || message.event?.workspaceId,
        status: message.status,
        reason: message.event?.reason,
        panels: (message.event?.resources || message.resources || [])
          .filter((resource) => resource.kind === "panel")
          .map((resource) => resource.agentId),
      });
      return {
        locks: Boolean(navigator.locks),
        channels: window.__syncChannels,
        timeline: window.__broadcastTimeline,
        receivedResourceEvents: window.__broadcastReceived.filter(
          (message) => message.kind === "resource-event",
        ),
        sentResourceEvents: window.__broadcastSent.filter(
          (message) => message.kind === "resource-event",
        ),
        postErrors: window.__broadcastPostErrors,
        received: window.__broadcastReceived.map(describe),
        sent: window.__broadcastSent.map(describe),
        sources: (window.__resourceEventSources || []).map((source) => ({
          url: source.__trackedUrl,
          opened: source.__opened,
          events: source.__resourceEvents.map((event) => ({
            reason: event.reason,
            panels: event.resources
              .filter((resource) => resource.kind === "panel")
              .map((resource) => resource.agentId),
          })),
        })),
      };
    });
    const ownerDiagnostics = await page.evaluate(() => ({
      locks: Boolean(navigator.locks),
      channels: window.__syncChannels,
      timeline: window.__broadcastTimeline,
      receivedResourceEvents: window.__broadcastReceived.filter(
        (message) => message.kind === "resource-event",
      ),
      sentResourceEvents: window.__broadcastSent.filter(
        (message) => message.kind === "resource-event",
      ),
      postErrors: window.__broadcastPostErrors,
      received: window.__broadcastReceived.map((message) => ({
        kind: message.kind,
        tabId: message.tabId,
        workspaceId: message.workspaceId || message.event?.workspaceId,
        panels: (message.resources || [])
          .filter((resource) => resource.kind === "panel")
          .map((resource) => resource.agentId),
      })),
      sent: window.__broadcastSent.map((message) => ({
        kind: message.kind,
        tabId: message.tabId,
        workspaceId: message.workspaceId || message.event?.workspaceId,
        reason: message.event?.reason,
        panels: (message.event?.resources || [])
          .filter((resource) => resource.kind === "panel")
          .map((resource) => resource.agentId),
      })),
    }));
    await expect
      .poll(() => followerPanelGets, {
        timeout: 1_000,
        message: JSON.stringify({ followerDiagnostics, ownerDiagnostics }),
      })
      .toContain(lead.id);
    expect(followerPanelGets).toContain(lead.id);
    expect((await watchState()).activeAgentIds).toEqual([lead.id]);

    // Let the baseline read and first size report settle. An idle panel must
    // not issue periodic reads or renew an unchanged layout measurement. This
    // spans both the former one-second panel poll and 30-second layout window.
    await page.waitForTimeout(1_500);
    const quietPanelGets = panelGets.length;
    const quietLayoutPosts = layoutPosts.length;
    await page.waitForTimeout(31_000);
    expect(panelGets.length).toBe(quietPanelGets);
    expect(layoutPosts.length).toBe(quietLayoutPosts);

    await page.bringToFront();
    await expect
      .poll(() => page.evaluate(() => document.visibilityState))
      .toBe("visible");
    const beforeResume = panelGets.length;
    const eventsBeforeResume = await page.evaluate(() =>
      (window.__resourceEventSources || []).reduce(
        (total, source) => total + source.__resourceEvents.length,
        0,
      ),
    );
    await page.evaluate(() => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
      window.dispatchEvent(new Event("offline"));
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: true,
      });
      window.dispatchEvent(new Event("online"));
      window.dispatchEvent(new PageTransitionEvent("pageshow"));
    });
    await page.waitForFunction(
      ({ previousCount, agentId }) => {
        const events = (window.__resourceEventSources || []).flatMap(
          (source) => source.__resourceEvents,
        );
        return (
          events.length > previousCount &&
          events
            .slice(previousCount)
            .some(
              (event) =>
                ["initial", "reconnect"].includes(event.reason) &&
                event.resources.some(
                  (resource) =>
                    resource.kind === "panel" && resource.agentId === agentId,
                ),
            )
        );
      },
      { previousCount: eventsBeforeResume, agentId: lead.id },
    );
    await expect.poll(() => panelGets.length).toBe(beforeResume + 1);
    await page.waitForTimeout(100);
    expect(panelGets.length).toBe(beforeResume + 1);
    expect(statusGets).toEqual([]);

    const beforeReplace = panelGets.length;
    const followerBeforeReplace = followerPanelGets.length;
    await rename(replacement, panel.path);
    await current()
      .getByText("Atomically replaced progress.", { exact: true })
      .waitFor();
    await followerCurrent()
      .getByText("Atomically replaced progress.", { exact: true })
      .waitFor();
    expect(panelGets.length).toBeGreaterThan(beforeReplace);
    expect(followerPanelGets.length).toBeGreaterThan(followerBeforeReplace);

    const beforeInvalidUtf8 = panelGets.length;
    await writeFile(replacement, Buffer.from([0xff]));
    await rename(replacement, panel.path);
    await expect
      .poll(async () => (await watchState()).errorAgentIds)
      .toContain(lead.id);
    const errorDetails = progress().getByRole("button", {
      name: /Cannot read PROGRESS\.md\. Error details|Error details/,
    });
    await errorDetails.waitFor();
    await errorDetails.click();
    const errorDialog = page.getByRole("dialog", { name: "Progress error" });
    await errorDialog
      .getByText("PROGRESS.md must contain valid UTF-8 text", { exact: true })
      .waitFor();
    await page.keyboard.press("Escape");
    await errorDialog.waitFor({ state: "hidden" });
    expect(panelGets.length).toBeGreaterThan(beforeInvalidUtf8);

    const beforeRecovery = panelGets.length;
    await writeFile(replacement, "Recovered progress.");
    await rename(replacement, panel.path);
    await current().getByText("Recovered progress.", { exact: true }).waitFor();
    expect(panelGets.length).toBeGreaterThan(beforeRecovery);

    const beforeDelete = panelGets.length;
    await unlink(panel.path);
    await progress().waitFor({ state: "detached" });
    expect(panelGets.length).toBeGreaterThan(beforeDelete);

    await page.locator(`[data-chat="${other.id}"]`).click();
    await page.waitForFunction(
      ({ leadId, otherId }) => {
        const entries = window.__resourceEventSources || [];
        return entries.some((source) => {
          const resources = JSON.parse(
            new URL(source.url, location.href).searchParams.get("resources") ||
              "[]",
          );
          return (
            resources.some(
              (resource) =>
                resource.kind === "panel" && resource.agentId === otherId,
            ) &&
            !resources.some(
              (resource) =>
                resource.kind === "panel" && resource.agentId === leadId,
            )
          );
        });
      },
      { leadId: lead.id, otherId: other.id },
    );
    await expect
      .poll(() =>
        streamResponses.some((response) => {
          const resources = JSON.parse(
            new URL(response.url).searchParams.get("resources") || "[]",
          );
          return (
            response.status === 200 &&
            resources.some(
              (resource) =>
                resource.kind === "panel" && resource.agentId === other.id,
            )
          );
        }),
      )
      .toBe(true);
    await expect
      .poll(() =>
        page.evaluate(
          (agentId) =>
            (window.__resourceEventSources || []).some((source) =>
              source.__resourceEvents.some(
                (event) =>
                  event.reason === "initial" &&
                  event.resources.some(
                    (resource) =>
                      resource.kind === "panel" && resource.agentId === agentId,
                  ),
              ),
            ),
          other.id,
        ),
      )
      .toBe(true);
    try {
      await expect.poll(watchState).toMatchObject({
        activeAgentIds: expect.arrayContaining([other.id]),
        dispatcherAlive: true,
        observerAlive: true,
      });
    } catch (error) {
      const streams = await page.evaluate(() =>
        (window.__resourceEventSources || []).map((source) => ({
          url: source.__trackedUrl,
          readyState: source.readyState,
          opened: source.__opened,
          errors: source.__errorCount,
          events: source.__resourceEvents,
        })),
      );
      throw new Error(
        `${error.message}\nSwitch evidence: ${JSON.stringify({
          streamResponses,
          streams,
          watchState: await watchState(),
          fixtureLog,
        })}`,
      );
    }
    expect((await watchState()).activeAgentIds).not.toContain(lead.id);
    expect(
      await page.evaluate(
        (leadId) =>
          (window.__resourceEventSources || []).some((source) => {
            const resources = JSON.parse(
              new URL(source.url, location.href).searchParams.get(
                "resources",
              ) || "[]",
            );
            return (
              source.__closedByClient &&
              resources.some(
                (resource) =>
                  resource.kind === "panel" && resource.agentId === leadId,
              )
            );
          }),
        lead.id,
      ),
    ).toBe(true);
    await expect.poll(watchState).toMatchObject({
      activeAgentIds: expect.arrayContaining([other.id]),
      dispatcherAlive: true,
      observerAlive: true,
    });
    await access(dirname(otherPanel.path));
    await expect(access(otherPanel.path)).rejects.toMatchObject({
      code: "ENOENT",
    });
    await writeFile(otherPanel.path, "Other active progress.");
    await current()
      .getByText("Other active progress.", { exact: true })
      .waitFor();
    await context.close();
    context = undefined;
    await expect.poll(watchState, { timeout: 5_000 }).toEqual({
      activeAgentIds: [],
      dispatcherAlive: false,
      observerAlive: false,
      errorAgentIds: [],
    });
  } finally {
    await context?.close();
    await rm(root, { recursive: true, force: true });
  }
});
