#!/usr/bin/env node
// Production UI with isolated HTTP reads. No model or user task writes.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
const repo = join(import.meta.dirname, "..");
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-messages-loading-"));
const fixture = spawn(
  process.env.PYTHON || "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
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
  const state = await (await fetch(origin + "/api/state")).json();
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
  const first = task("first-needed-task", lead, "Confirm the release account");
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
  const workScopes = [];
  let workDelay = 0;
  let stateReads = 0,
    workspaceReads = 0,
    detailReads = 0,
    roomReads = 0,
    roomFails = true;
  browser = await engine.launch({
    headless: true,
    ...(engine === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
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
    await next.route("**/api/sync/**", (route) =>
      route.fulfill({ status: 404, json: { error: "Use fixture polling" } }),
    );
    await next.route("**/api/state*", (route) => {
      stateReads++;
      const current = structuredClone(state);
      current.fixtureRevision = stateReads;
      current.threads[0].updated = 100 + stateReads;
      return route.fulfill({ json: current });
    });
    await next.route("**/api/workspace?*", async (route) => {
      workspaceReads++;
      await pause(8000);
      await route
        .fulfill({
          status: 503,
          json: { error: "Workspace history unavailable" },
        })
        .catch(() => {});
    });
    await next.route("**/api/work?*", async (route) => {
      workScopes.push(new URL(route.request().url()).searchParams.get("agent"));
      if (workDelay) await pause(workDelay);
      return route.fulfill({ json: { tasks: [review] } }).catch(() => {});
    });
    await next.route("**/api/complaint?*", async (route) => {
      detailReads++;
      await pause(3500);
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
  await drawer
    .getByText(second.title, { exact: true })
    .waitFor({ timeout: 1500 });
  assert.equal(
    workspaceReads,
    0,
    "For you uses the current snapshot without a workspace history request",
  );
  assert.equal(
    await drawer.getByText("Unrelated private task", { exact: true }).count(),
    0,
  );
  await drawer.getByText(review.title, { exact: true }).waitFor();
  assert.ok(
    workScopes.length > 0,
    "missing chat work history uses a separate scoped read",
  );
  assert.ok(
    workScopes.every((id) => id === lead.id),
    "work review reads stay in the selected lead",
  );
  workDelay = 2000;
  await drawer.getByRole("tab", { name: "Team", exact: true }).click();
  await drawer.getByRole("tab", { name: "For you", exact: true }).click();
  await drawer
    .getByText(review.title, { exact: true })
    .waitFor({ timeout: 500 });
  assert.equal(
    await drawer.getByText("Checking review tasks…", { exact: true }).count(),
    0,
    "returning to the inbox keeps the last confirmed data during a slow refresh",
  );
  workDelay = 0;
  await drawer.getByText(second.title, { exact: true }).click();
  const focused = page.locator(
    `[data-user-task="${second.id}"].user-task-focused`,
  );
  await focused
    .getByRole("textbox", { name: "Result note (optional)", exact: true })
    .waitFor();
  assert.equal(
    await focused
      .getByRole("button", { name: "Send for review", exact: true })
      .count(),
    1,
  );
  assert.equal(
    await page.locator(`[data-user-task="${first.id}"] textarea`).count(),
    0,
    "the exact requested task opens",
  );
  await page
    .getByRole("dialog", { name: "Workspace", exact: true })
    .locator(".mantine-Drawer-close")
    .click();
  drawer = await openMessages(page);
  const readsBefore = stateReads;
  await drawer.locator(`[data-complaint="${complaint.id}"]`).click();
  const modal = page.getByRole("dialog", {
    name: "Message to you",
    exact: true,
  });
  await modal.getByRole("textbox", { name: "Reply", exact: true }).waitFor();
  assert.ok(
    stateReads - readsBefore >= 2,
    "state updates while the detail request is in flight",
  );
  assert.equal(
    detailReads,
    1,
    "state object changes do not replace the detail request",
  );
  await modal.getByRole("button", { name: "Close", exact: true }).click();
  await drawer.getByRole("tab", { name: "Team", exact: true }).click();
  const roomDetail = drawer.locator(`[data-room-detail="${broadcast.id}"]`);
  await roomDetail
    .getByRole("alert")
    .filter({ hasText: "Room read unavailable" })
    .waitFor();
  assert.equal(
    await roomDetail.getByText("No messages yet.", { exact: true }).count(),
    0,
  );
  assert.equal(
    await drawer
      .locator(`[data-room="${broadcast.id}"]`)
      .getAttribute("aria-pressed"),
    "true",
    "desktop opens the most recent room",
  );
  roomFails = false;
  await roomDetail
    .getByRole("button", { name: "Retry messages", exact: true })
    .click();
  await roomDetail.locator(".team-message").waitFor();
  await roomDetail.getByRole("alert").waitFor({ state: "hidden" });
  await page.screenshot({
    path: join(root, "messages-desktop.png"),
    animations: "disabled",
  });
  const mobile = await prepare(390);
  const mobileDrawer = await openMessages(mobile);
  await mobileDrawer.getByRole("tab", { name: "Team", exact: true }).click();
  await mobileDrawer
    .getByRole("textbox", { name: "Search team chats", exact: true })
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
  const mobileLayout = await mobileDrawer.evaluate((element) =>
    Object.fromEntries(
      [
        ".mantine-Drawer-content",
        ".mantine-Drawer-body",
        ".workspace-focused-messages",
        ".workspace-messages",
        ".workspace-message-body",
        ".messages-views",
        ".messages-team-panel",
        ".team-chats",
        ".team-room-detail",
        ".team-room-footer",
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
    mobileLayout[".team-room-footer"].bottom >= 896,
    "the room fills the mobile window to its bottom edge",
  );
  assert.ok(
    mobileLayout[".team-room-footer"].bottom <= 901,
    "the room footer stays inside the mobile window",
  );
  await mobile.screenshot({
    path: join(root, "messages-mobile.png"),
    animations: "disabled",
  });
  await mobileDrawer
    .getByRole("button", { name: "Back to team chats", exact: true })
    .click();
  await mobileDrawer
    .getByRole("textbox", { name: "Search team chats", exact: true })
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
        "snapshot inbox without workspace read",
        "exact user task focus",
        "work review absent from chat snapshot",
        "cached inbox survives a slow tab return",
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
  fixture.kill("SIGTERM");
}
