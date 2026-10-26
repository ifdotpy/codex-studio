import type { Snapshot } from "../../src/types.ts";
import type { components } from "../../src/generated/api.ts";
type PeerRequest =
  | components["schemas"]["PeerTeamMoveRequest"]
  | components["schemas"]["PeerTeamDeleteRequest"]
  | components["schemas"]["PeerTeamSaveRequest"];
import type {} from "./peer-teams-browser.fixture.tsx";
import { required } from "./helpers.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { apiSchemaHeaders, test } from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
test("Peer teams browser", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = resolve(import.meta.dirname, "../../");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-peer-teams-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "fixture",
        configureServer(s) {
          s.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<html><body><div id="root"></div><script type="module" src="/tests/teams/peer-teams-browser.fixture.tsx"></script></body></html>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 1100, height: 900 });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    let state: Snapshot,
      mode = "lost",
      requests: PeerRequest[] = [];
    await page.route("**/snapshot", (r) => r.fulfill({ json: state }));
    const applied = new Map<string, PeerRequest>();
    await page.route("**/api/peer-teams", async (route) => {
      const body: PeerRequest = route.request().postDataJSON();
      requests.push(body);
      if (mode === "stale")
        return route.fulfill({
          status: 409,
          json: { error: "Project changed" },
          headers: apiSchemaHeaders(),
        });
      if (applied.has(body.request_id)) {
        assert.deepEqual(body, applied.get(body.request_id));
        return route.fulfill({
          json: { ok: true },
          headers: apiSchemaHeaders(),
        });
      }
      if (body.action === "move") {
        const target = state.runtime.peerTeams.find(
          (t) => t.id === body.team_id,
        );
        for (const t of state.runtime.peerTeams)
          t.members = (t.members ?? []).filter((id) => id !== body.member);
        if (target) (target.members ??= []).push(body.member);
        state.runtime.peerTeams = state.runtime.peerTeams.filter(
          (t) => (t.members ?? []).length >= 2,
        );
      } else
        state.runtime.peerTeams =
          body.action === "delete"
            ? []
            : [
                {
                  id: body.team_id,
                  name: body.name,
                  projectPath: body.path,
                  members: body.members,
                },
              ];
      const project = required(state.runtime.projects[0]);
      project.peerTeamsRevision = (project.peerTeamsRevision ?? 0) + 1;
      applied.set(body.request_id, body);
      if (mode === "lost") {
        mode = "ok";
        return route.abort("failed");
      }
      return route.fulfill({
        json: { ok: true },
        headers: apiSchemaHeaders(),
      });
    });
    await page.goto(`${required(server.resolvedUrls?.local[0])}check`);
    await page.waitForFunction(() => window.peerTeamsFixture.data);
    state = await page.evaluate(() => window.peerTeamsFixture.data);
    const project = page.locator('[data-project-path="/p"]');
    const menu = async () => {
      await project.locator(":scope > .project-tree-heading").hover();
      await page
        .getByRole("button", {
          name: "Options for project Project",
          exact: true,
        })
        .click();
    };
    await menu();
    await page.getByRole("menuitem", { name: "New team", exact: true }).click();
    let form = page.getByRole("form", { name: "New team", exact: true });
    assert.deepEqual(await form.locator("input[type=checkbox]").count(), 3);
    await form.getByLabel("Team name").fill("Equal team");
    await form.getByLabel("Alpha", { exact: true }).check();
    await form.getByLabel("Beta", { exact: true }).check();
    await form.getByRole("button", { name: "Save team", exact: true }).click();
    await form.getByRole("alert").waitFor();
    await form.getByRole("button", { name: "Retry", exact: true }).click();
    await form.waitFor({ state: "detached" });
    assert.deepEqual(requests[0], requests[1]);
    assert.equal(required(state.runtime.projects[0]).peerTeamsRevision, 1);
    await page.getByRole("button", { name: /^Show old \(/ }).click();
    assert.equal(await project.locator('[data-chat="a"]').count(), 1);
    assert.equal(await project.locator('[data-chat="b"]').count(), 1);
    assert.equal(
      await project.locator('[data-folder-id="f"] [data-chat="b"]').count(),
      0,
    );
    const team = page.locator("[data-peer-team]");
    await team.getByRole("button", { name: "Equal team", exact: true }).click();
    assert.equal(await team.locator('[data-chat="a"]').count(), 1);
    assert.equal(await team.locator('[data-chat="b"]').count(), 0);
    await page.getByLabel("Filter projects and chats").fill("no match");
    assert.equal(await team.locator('[data-chat="a"]').count(), 1);
    await page.getByLabel("Filter projects and chats").fill("");
    const teamMenu = async () => {
      await team.locator(":scope > .project-tree-heading").hover();
      await team
        .getByRole("button", {
          name: "Options for team Equal team",
          exact: true,
        })
        .click();
    };
    await teamMenu();
    await page
      .getByRole("menuitem", { name: "Edit team", exact: true })
      .click();
    form = page.getByRole("form", { name: "Edit team", exact: true });
    await form.getByLabel("Gamma", { exact: true }).check();
    mode = "stale";
    await form.getByRole("button", { name: "Save team", exact: true }).click();
    await form.getByRole("alert").waitFor();
    assert.match(await form.innerText(), /Project changed/);
    mode = "ok";
    await form.getByRole("button", { name: "Reload project" }).click();
    await form.waitFor({ state: "detached" });
    await teamMenu();
    await page
      .getByRole("menuitem", { name: "Edit team", exact: true })
      .click();
    form = page.getByRole("form", { name: "Edit team", exact: true });
    await form.getByLabel("Gamma", { exact: true }).check();
    await page.evaluate(() => (window.peerTeamsFixture.failRefresh = true));
    await form.getByRole("button", { name: "Save team", exact: true }).click();
    await form.getByRole("alert").waitFor();
    assert.match(await form.innerText(), /change was saved/);
    const count = requests.length;
    await page.evaluate(() => (window.peerTeamsFixture.failRefresh = false));
    await form.getByRole("button", { name: "Retry", exact: true }).click();
    await form.waitFor({ state: "detached" });
    assert.equal(requests.length, count);
    await teamMenu();
    await page
      .getByRole("menuitem", { name: "Dissolve team", exact: true })
      .click();
    form = page.getByRole("form", { name: "Dissolve team", exact: true });
    await form.getByRole("button", { name: "Dissolve", exact: true }).click();
    await form.waitFor({ state: "detached" });
    assert.equal(await team.count(), 0);
    assert.equal(
      await project.locator('[data-folder-id="f"] [data-chat="b"]').count(),
      1,
    );
    state.threads.push({
      ...required(state.threads.find((a) => a.id === "c")),
      id: "e",
      name: "Epsilon",
    });
    state.runtime.peerTeams = [
      { id: "t1", name: "First team", projectPath: "/p", members: ["a", "b"] },
      { id: "t2", name: "Second team", projectPath: "/p", members: ["c", "e"] },
    ];
    await page.evaluate((serialized) => {
      // The fixture serializes its own snapshot across the browser boundary.
      const snapshot: Snapshot = JSON.parse(serialized);
      window.peerTeamsFixture.set(snapshot);
    }, JSON.stringify(state));
    const second = page.locator('[data-peer-team="t2"]');
    const teamTarget = second.locator(".peer-team-toggle");
    const projectTarget = project.locator(
      ":scope > .project-tree-heading .project-tree-toggle",
    );
    await project.locator('[data-chat="a"]').dragTo(teamTarget);
    await second.locator('[data-chat="a"]').waitFor();
    assert.equal(await page.locator('[data-peer-team="t1"]').count(), 0);
    assert.equal(required(requests.at(-1)).action, "move");
    await second.locator('[data-chat="a"]').dragTo(projectTarget);
    await page.waitForFunction(
      () =>
        !window.peerTeamsFixture.data.runtime.peerTeams.some((t) =>
          t.members?.includes("a"),
        ),
    );
    const beforeForeign = requests.length;
    await page.locator('[data-chat="x"]').dragTo(teamTarget);
    assert.equal(
      requests.length,
      beforeForeign,
      "Foreign projects cannot join the team",
    );
    mode = "lost";
    await project.locator('[data-chat="b"]').dragTo(teamTarget);
    await page
      .getByRole("button", { name: "Retry move", exact: true })
      .waitFor();
    const lostMove = requests.at(-1);
    await page.getByRole("button", { name: "Retry move", exact: true }).click();
    await second.locator('[data-chat="b"]').waitFor();
    assert.deepEqual(
      requests.at(-1),
      lostMove,
      "Retry keeps the move identity and original revision",
    );
    mode = "stale";
    await second.locator('[data-chat="b"]').dragTo(projectTarget);
    await page
      .getByRole("button", { name: "Reload project", exact: true })
      .waitFor();
    mode = "ok";
    await page
      .getByRole("button", { name: "Reload project", exact: true })
      .click();
    await second.locator('[data-chat="b"]').dragTo(projectTarget);
    await page.waitForFunction(
      () =>
        !window.peerTeamsFixture.data.runtime.peerTeams.some((t) =>
          t.members?.includes("b"),
        ),
    );
    await page.screenshot({
      path:
        process.env.PEER_TEAMS_DRAG_SCREENSHOT ||
        join(cacheDir, "studio-team-drag.png"),
    });
    state.runtime.peerTeams = [];
    // Only explicitly authorized peer rooms involving this chat appear.
    state.runtime.rooms = [
      {
        id: "peer",
        name: "Peer",
        kind: "private",
        members: ["a", "b"],
        peerTeamId: "t",
        updated: 1,
      },
      {
        id: "other",
        name: "Other peers",
        kind: "private",
        members: ["b", "c"],
        peerTeamId: "t",
        updated: 1,
      },
      {
        id: "unmarked",
        name: "Unmarked",
        kind: "private",
        members: ["a", "c"],
        updated: 1,
      },
    ];
    await page.evaluate((serialized) => {
      // The fixture serializes its own snapshot across the browser boundary.
      const snapshot: Snapshot = JSON.parse(serialized);
      window.peerTeamsFixture.set(snapshot);
    }, JSON.stringify(state));
    await page.locator('[data-room="peer"]').waitFor();
    assert.equal(await page.locator('[data-room="other"]').count(), 0);
    assert.equal(await page.locator('[data-room="unmarked"]').count(), 0);
    await page.setViewportSize({ width: 360, height: 800 });
    await menu();
    await page.getByRole("menuitem", { name: "New team", exact: true }).click();
    form = page.getByRole("form", { name: "New team", exact: true });
    await form
      .getByLabel("Team name")
      .fill("A long team name that should fit a narrow sidebar");
    const bounds = required(await form.boundingBox());
    assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= 360);
    assert.equal(
      await form.evaluate((e) => e.scrollWidth <= e.clientWidth),
      true,
    );
    await page.screenshot({
      path: process.env.PEER_TEAMS_SCREENSHOT || join(cacheDir, "narrow.png"),
    });
    assert.deepEqual(errors, []);
    console.log(
      "peer teams UI: create, retry, edit, stale revision, refresh failure, dissolve, room privacy, narrow viewport PASS",
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
