import {
  readTestState,
  entityPullFixtureForRequest,
  entityPullFixture,
  syncIdentityFixture,
  test,
  browserExecutablePath,
  spawnFixture as spawn,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
test("Messages loading", async () => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Production UI with isolated HTTP reads. No model or user task writes.
  const repo = testRepo;
  const { chromium, webkit } = createRequire(
    join(repo, "workspaces/client/apps/web/package.json"),
  )("playwright-core");
  const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
  const root = await mkdtemp(join(tmpdir(), "studio-messages-loading-"));
  const fixture = spawn(
    process.env.PYTHON || "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        MESSAGES_UI_FIXTURE: "1",
        CODEX_BOARD_STATE_DIR: join(root, "board"),
      },
    },
  );
  let browser,
    page,
    log = "";
  fixture.stderr.on("data", (value) => {
    log += value;
  });
  const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (value) =>
        resolve(Number(String(value).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.threads.find((agent) => agent.name === "Release lead");
    const other = state.threads.find((agent) => agent.name === "Other project");
    const complaint = state.runtime.complaints.find(
      (item) => item.author === lead.id,
    );
    const detail = await (
      await fetch(
        `${origin}/api/complaint?id=${encodeURIComponent(complaint.id)}`,
      )
    ).json();
    const inboxComplaint = { ...complaint };
    delete inboxComplaint.text;
    delete inboxComplaint.responses;
    state.runtime.complaints = [inboxComplaint];
    const direct = state.runtime.rooms.find(
      (room) => room.kind === "private" && room.members.includes(lead.id),
    );
    const broadcast = state.runtime.rooms.find(
      (room) => room.kind === "broadcast" && room.rootId === lead.id,
    );
    direct.updated = 100;
    direct.lastMessage = { ...direct.lastMessage, created: 100 };
    broadcast.updated = 200;
    broadcast.lastMessage = { ...broadcast.lastMessage, created: 200 };
    state.runtime.rooms = [direct, broadcast];
    state.runtime.requests = [];
    state.runtime.monitors = [];
    state.runtime.tasks = [];
    delete state.runtime.work;
    const review = {
      id: "pending-work-review",
      rootId: lead.id,
      title: "Review the release evidence",
      status: "review",
      created: 100,
      updated: 100,
    };
    const task = (id, agent, title) => ({
      id,
      agent: agent.id,
      rootId: agent.rootId,
      title,
      description: `Instructions for ${id}.`,
      criteria: `Evidence for ${id}.`,
      status: "open",
      version: 1,
      created: 100,
      updated: 100,
      reason: "",
      completionNote: "",
      history: [],
    });
    const first = task(
      "first-needed-task",
      lead,
      "Confirm the release account",
    );
    const second = task(
      "second-needed-task",
      lead,
      "Confirm the deployment window",
    );
    state.runtime.userTasks = [
      first,
      second,
      task("other-team-task", other, "Unrelated private task"),
    ];
    for (const agent of state.threads)
      Object.assign(agent, {
        status: "completed",
        inFlight: false,
        turnId: null,
      });
    state.runtime.agents = state.threads;
    const markdown =
      "Message content remains available.\n\n```text\n" +
      "long_command_".repeat(100) +
      "\n```\n\n| Column | Value |\n| --- | --- |\n| Evidence | " +
      "WideTableValue".repeat(60) +
      " |";
    const roomPayload = (room) => ({
      room,
      nextBefore: null,
      messages: [
        {
          id: `message-${room.id}`,
          seq: 1,
          sender: room.members[0],
          senderName: "Reviewer",
          text: markdown,
          created: 100,
          deliveries: {},
        },
      ],
    });
    const errors = [];
    let stateReads = 0,
      workspaceReads = 0,
      detailReads = 0,
      roomReads = 0,
      roomFails = true,
      holdNextComplaintDetail = false,
      releaseComplaintDetail;
    browser = await engine.launch({
      headless: true,
      ...(engine === chromium
        ? {
            executablePath: browserExecutablePath,
          }
        : {}),
    });
    const prepare = async (width) => {
      const next = await browser.newPage({
        viewport: { width, height: 900 },
        serviceWorkers: "block",
      });
      next.setDefaultTimeout(12000);
      next.on("pageerror", (error) => errors.push(error.message));
      const backendIdentity = await (
        await fetch(`${origin}/api/sync/identity`)
      ).json();
      const identity = syncIdentityFixture(backendIdentity.workspaceId);
      await next.route("**/api/sync/identity", (route) =>
        route.fulfill({ json: identity }),
      );
      await next.route("**/api/sync/pull?*", (route) => {
        stateReads++;
        const current = structuredClone(state);
        current.fixtureRevision = stateReads;
        const leadEntity = current.runtime.agents.find(
          (agent) => agent.id === lead.id,
        );
        if (leadEntity) leadEntity.updated = 100 + stateReads;
        const requestUrl = route.request().url();
        const projection = entityPullFixtureForRequest(current, requestUrl);
        const parsedUrl = new URL(requestUrl);
        if (
          parsedUrl.searchParams.get("scope") === "state:entities:v1" &&
          projection.documents.length === 0 &&
          Number(parsedUrl.searchParams.get("after") || 0) >= projection.maxSeq
        ) {
          const initial = entityPullFixture(current, {
            scope: "state:entities:v1",
            fresh: true,
          });
          const update = initial.documents.find(
            (document) => document.id === `entity:agent:${lead.id}`,
          );
          if (update) {
            const seq = projection.maxSeq + 1;
            projection.documents.push({ ...update, seq });
            projection.checkpoint.seq = seq;
            projection.maxSeq = seq;
          }
        }
        return route.fulfill({
          json: {
            workspaceId: identity.workspaceId,
            ...projection,
          },
        });
      });
      await next.route("**/api/workspace/tasks?*", async (route) => {
        workspaceReads++;
        await pause(8000);
        await route
          .fulfill({
            status: 503,
            json: { error: "Workspace history unavailable" },
          })
          .catch(() => {});
      });
      await next.route("**/api/complaint?*", async (route) => {
        detailReads++;
        if (holdNextComplaintDetail) {
          holdNextComplaintDetail = false;
          await new Promise((resolve) => {
            releaseComplaintDetail = resolve;
          });
        } else {
          await pause(3500);
        }
        await route.fulfill({ json: detail });
      });
      await next.route("**/api/agent-chat?*", (route) => {
        roomReads++;
        if (roomFails)
          return route.fulfill({
            status: 503,
            json: { error: "Room read unavailable" },
          });
        const id = new URL(route.request().url()).searchParams.get("room");
        return route.fulfill({
          json: roomPayload(id === direct.id ? direct : broadcast),
        });
      });
      await next.addInitScript(
        ({ stateDir, id }) => {
          localStorage.setItem(
            `codex-desktop-opened:${stateDir}`,
            JSON.stringify(id),
          );
          localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
        },
        { stateDir: state.stateDir, id: lead.id },
      );
      await next.goto(origin);
      await next
        .locator("#conversation-title")
        .filter({ hasText: lead.name })
        .waitFor();
      return next;
    };
    const openMessages = async (target) => {
      if (await target.locator("#messages-toggle").isVisible())
        await target.locator("#messages-toggle").click();
      else {
        await target
          .getByRole("button", { name: "Chat settings", exact: true })
          .click();
        await target
          .getByRole("dialog", { name: "Chat settings" })
          .getByRole("button", { name: "Messages", exact: true })
          .click();
      }
      const dialog = target.getByRole("dialog", {
        name: "Messages",
        exact: true,
      });
      await dialog.waitFor();
      return dialog;
    };
    page = await prepare(1280);
    let drawer = await openMessages(page);
    assert.equal(
      await drawer.locator('[data-room="you"]').getAttribute("aria-pressed"),
      "true",
      "the Messages view opens on the For you inbox",
    );
    assert.equal(
      await drawer.getByText(second.title, { exact: true }).count(),
      0,
    );
    assert.equal(
      workspaceReads,
      0,
      "the current Messages inbox does not fetch removed workspace task history",
    );
    assert.equal(
      await drawer.getByText("Unrelated private task", { exact: true }).count(),
      0,
    );
    assert.equal(
      await drawer.getByText(review.title, { exact: true }).count(),
      0,
    );
    const complaintCard = drawer.locator(`[data-complaint="${complaint.id}"]`);
    await complaintCard
      .getByText("Loading message…", { exact: true })
      .waitFor();
    await complaintCard.getByText(detail.text, { exact: true }).waitFor();
    assert.equal(detailReads, 1, "one summary starts one detail request");
    await complaintCard
      .getByRole("button", { name: "Reply", exact: true })
      .click();
    await complaintCard
      .getByRole("textbox", { name: "Reply", exact: true })
      .waitFor();
    await drawer.locator(`[data-room="${direct.id}"]`).click();
    holdNextComplaintDetail = true;
    const readsBeforeRefresh = stateReads;
    await drawer.locator(`[data-room="you"]`).click();
    await drawer
      .getByText("Loading message…", { exact: true })
      .waitFor({ timeout: 500 });
    assert.equal(
      detailReads,
      2,
      "returning to For you requests one fresh detail after remount",
    );
    const refreshedName = `${lead.name} refreshed`;
    lead.name = refreshedName;
    for (const row of state.threads)
      if (row.id === lead.id) row.name = refreshedName;
    for (const row of state.runtime.agents)
      if (row.id === lead.id) row.name = refreshedName;
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await page
      .locator("#conversation-title")
      .filter({ hasText: refreshedName })
      .waitFor({ timeout: 6000 });
    assert.ok(
      stateReads > readsBeforeRefresh,
      "the supported resume refresh reads the updated state while complaint detail is held",
    );
    assert.equal(
      await drawer.getByText("Loading message…", { exact: true }).count(),
      1,
      "state refresh does not wait for or replace the held complaint detail",
    );
    assert.equal(typeof releaseComplaintDetail, "function");
    releaseComplaintDetail();
    await complaintCard.getByText(detail.text, { exact: true }).waitFor();
    const roomDetail = drawer.locator(".team-room-detail");
    await drawer.locator(`[data-room="${broadcast.id}"]`).click();
    await roomDetail
      .getByRole("alert")
      .filter({ hasText: "Room read unavailable" })
      .waitFor();
    assert.equal(
      await roomDetail.getByText("No messages yet.", { exact: true }).count(),
      1,
      "a failed initial read shows the empty state with its retry notice",
    );
    assert.equal(
      await drawer
        .locator(`[data-room="${broadcast.id}"]`)
        .getAttribute("aria-pressed"),
      "true",
      "selecting the broadcast opens that room",
    );
    roomFails = false;
    await roomDetail
      .getByRole("button", { name: "Retry", exact: true })
      .click();
    await roomDetail.locator(".team-message").waitFor();
    await roomDetail.getByRole("alert").waitFor({ state: "hidden" });
    await page.screenshot({
      path: join(root, "messages-desktop.png"),
      animations: "disabled",
    });
    const mobile = await prepare(390);
    const mobileDrawer = await openMessages(mobile);
    await mobileDrawer
      .getByRole("textbox", { name: "Search chats", exact: true })
      .waitFor();
    assert.equal(
      await mobileDrawer.locator(".team-room-detail").isVisible(),
      false,
      "mobile starts at the room list",
    );
    await mobileDrawer.locator(`[data-room="${direct.id}"]`).click();
    await mobileDrawer.locator(".team-message").waitFor();
    const overflow = await mobileDrawer.evaluate((element) =>
      [
        ...element.querySelectorAll(
          ".team-room-detail,.team-room-messages,.team-message",
        ),
      ].some((node) => node.scrollWidth > node.clientWidth + 1),
    );
    assert.equal(
      overflow,
      false,
      "long code and tables stay inside the mobile conversation",
    );
    const mobileLayout = await mobileDrawer.evaluate((_element) =>
      Object.fromEntries(
        [
          ".mantine-Drawer-content",
          ".mantine-Drawer-body",
          ".team-chats",
          ".team-room-detail",
          ".team-room-header",
          ".unified-message-scroll",
        ].map((selector) => {
          const node = document.querySelector(selector);
          if (!node) return [selector, null];
          const rect = node.getBoundingClientRect();
          const style = getComputedStyle(node);
          return [
            selector,
            {
              height: rect.height,
              top: rect.top,
              bottom: rect.bottom,
              display: style.display,
              flex: style.flex,
              flexDirection: style.flexDirection,
              cssHeight: style.height,
              minHeight: style.minHeight,
            },
          ];
        }),
      ),
    );
    await writeFile(
      join(root, "mobile-layout.json"),
      JSON.stringify(mobileLayout, null, 2),
    );
    assert.ok(
      mobileLayout[".team-room-detail"].bottom >= 896,
      "the room detail fills the mobile window to its bottom edge",
    );
    assert.ok(
      mobileLayout[".team-room-detail"].bottom <= 901,
      "the room detail stays inside the mobile window",
    );
    await mobile.screenshot({
      path: join(root, "messages-mobile.png"),
      animations: "disabled",
    });
    await mobileDrawer
      .getByRole("button", { name: "Back to chats", exact: true })
      .click();
    await mobileDrawer
      .getByRole("textbox", { name: "Search chats", exact: true })
      .waitFor();
    assert.equal(
      await mobileDrawer.locator(".team-room-detail").isVisible(),
      false,
    );
    assert.equal(workspaceReads, 0);
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        ok: true,
        evidence: root,
        engine: process.env.BROWSER || "chromium",
        stateReads,
        detailReads,
        roomReads,
        cases: [
          "current inbox excludes removed user-task history without workspace read",
          "complaint detail waits without blocking state refresh",
          "confirmed message is retained while a remount refresh is pending",
          "slow detail survives state updates",
          "desktop latest room",
          "room retry clears error",
          "mobile list and back",
          "markdown overflow",
        ],
      }),
    );
  } catch (error) {
    await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
    console.error("Evidence:", root, error);
    throw error;
  } finally {
    await browser?.close();
  }
});
