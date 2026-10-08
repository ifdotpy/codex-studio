#!/usr/bin/env node
import {
  test,
  expect,
  spawnFixture as spawn,
  readTestState,
  entityPullFixture,
} from "../playwright.mjs";
// Production renderer with an isolated fixture. No model calls or user state.

import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
test("Worker Overview Ui", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "codex-worker-overview-"));
  const proc = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
    },
  );
  let page,
    log = "";
  proc.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(Error("Fixture timeout\n" + log)),
        30000,
      );
      proc.stdout.once("data", (data) => {
        clearTimeout(timer);
        resolve(Number(String(data).trim()));
      });
      proc.once("exit", () => {
        clearTimeout(timer);
        reject(Error(log));
      });
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await readTestState(origin);
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const workers = initial.threads.filter(
      (agent) => agent.rootId === lead.id && !agent.isLead,
    );
    const worker = (n) =>
      workers.find(
        (agent) => agent.name === `Worker ${String(n).padStart(2, "0")}`,
      );
    const resultPath = join(root, "results", "task-one", "result-one.md");
    await mkdir(join(initial.stateDir, "progress", worker(25).id), {
      recursive: true,
    });
    await writeFile(
      join(initial.stateDir, "progress", worker(25).id, "PROGRESS.md"),
      "This worker progress file must not appear in its chat.\n",
    );
    const task =
      "Check the exact response receipt before any repeated mutation. " +
      "This is a long assignment with complete instructions. "
        .repeat(14)
        .trim();
    const report =
      "Verified receipt recovery without duplicate work. " +
      "All observed cases and their evidence remain in the report. "
        .repeat(10)
        .trim();
    const longName =
      "Review the continuation receipt across concurrent requests and uncertain responses";
    let workerFailure = {
      additionalDetails: "The account quota is exhausted.",
      codexErrorInfo: "usageLimitExceeded",
      message: "The usage limit was reached. Try again after the reset.",
      misalignment: null,
    };
    const stopReason = "Stopped by agent Release lead";
    let workerStopError = stopReason;
    let pending = true;
    let deferred = false;
    let fixtureRefreshStage = 0;
    let appliedFixtureRefreshStage = 0;
    page = testPage;
    await page.setViewportSize({ width: 1440, height: 980 });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      console.error("Browser error:", error.message);
    });
    // Keep the real fixture stream open; only pull values are customized below.
    await page.route(/\/api\/file\?/, (route) =>
      route.fulfill({
        json: {
          name: "result-one.md",
          mime: "text/markdown",
          base64: Buffer.from("# Submitted result\n\nVerified.").toString(
            "base64",
          ),
        },
      }),
    );
    await page.route("**/api/sync/pull?*", async (route) => {
      const response = await route.fetch();
      const data = await response.json();
      const pullUrl = new URL(route.request().url());
      const isEntityPull =
        pullUrl.searchParams.get("scope") === "state:entities:v1";
      for (const document of data.documents ?? []) {
        if (document._deleted) continue;
        const entity = JSON.parse(document.payload);
        if (entity.collection === "agent") {
          const agent = entity.value;
          const workerIndex = workers.findIndex(
            (worker) => worker.id === agent.id,
          );
          if (workerIndex >= 0) {
            const index = Number(workers[workerIndex].name.split(" ")[1]);
            agent.status =
              index === 7
                ? "failed"
                : index < 8
                  ? "running"
                  : index === 24
                    ? "paused"
                    : index < 25
                      ? "queued"
                      : "completed";
          }
          if (agent.id === worker(7).id) agent.error = workerFailure;
          if (agent.id === worker(24).id)
            Object.assign(agent, { autoWake: false, error: workerStopError });
          if (agent.id === worker(0).id && deferred) agent.status = "approval";
          if (agent.id === worker(1).id)
            Object.assign(agent, {
              name: longName,
              model: "gpt-6-astra",
              effort: "high",
              overview: { task, result: "" },
            });
          if (agent.id === worker(25).id)
            Object.assign(agent, {
              overview: {
                task: "Verify receipt recovery",
                result: report,
                resultTruncated: true,
                resultFile: resultPath,
              },
            });
          entity.value = agent;
        }
        document.payload = JSON.stringify(entity);
      }
      if (isEntityPull) {
        const requests = [];
        if (pending)
          requests.push({
            id: "worker-question",
            agent: worker(0).id,
            status: "pending",
            deferred,
            method: "agent/asyncQuestion",
            params: {
              questions: [
                { id: "scope", question: "Which receipt should I inspect?" },
              ],
            },
          });
        requests.push({
          id: "answered-worker-question",
          agent: worker(2).id,
          status: "answered",
          method: "agent/asyncQuestion",
          params: {
            questions: [{ id: "done", question: "Already answered" }],
          },
        });
        let seq = data.checkpoint.seq;
        data.documents.push(
          ...requests.map((value) => {
            seq += 1;
            return {
              id: `entity:request:${value.id}`,
              seq,
              _deleted: false,
              payload: JSON.stringify({
                collection: "request",
                id: value.id,
                value,
              }),
            };
          }),
        );
        data.checkpoint.seq = seq;
        data.maxSeq = seq;
        if (!pending) {
          const tombstoneSeq = data.maxSeq + 1;
          data.documents.push({
            id: "entity:request:worker-question",
            seq: tombstoneSeq,
            _deleted: true,
            payload: "{}",
          });
          data.checkpoint.seq = tombstoneSeq;
          data.maxSeq = tombstoneSeq;
        }
      }
      if (isEntityPull && fixtureRefreshStage > appliedFixtureRefreshStage) {
        appliedFixtureRefreshStage = fixtureRefreshStage;
        const changedAgents = initial.runtime.agents.map((agent) => ({
          ...agent,
        }));
        const updateAgent = (id, updates) => {
          const index = changedAgents.findIndex((agent) => agent.id === id);
          if (index >= 0)
            changedAgents[index] = { ...changedAgents[index], ...updates };
        };
        updateAgent(worker(24).id, {
          status: "failed",
          autoWake: false,
          error: workerStopError,
        });
        updateAgent(worker(7).id, { status: "failed", error: workerFailure });
        const changedState = {
          ...initial,
          threads: changedAgents,
          runtime: { ...initial.runtime, agents: changedAgents },
        };
        const projected = entityPullFixture(changedState, {
          scope: "state:entities:v1",
          fresh: true,
        });
        let seq = data.maxSeq;
        for (const id of [worker(24).id, worker(7).id]) {
          const source = projected.documents.find(
            (document) => document.id === `entity:agent:${id}`,
          );
          if (!source) continue;
          seq++;
          data.documents.push({ ...source, seq });
        }
        data.checkpoint.seq = seq;
        data.maxSeq = seq;
      }
      await route.fulfill({ response, json: data });
    });
    await page.addInitScript(() => {
      const NativeEventSource = window.EventSource;
      window.EventSource = class extends NativeEventSource {
        constructor(url, options) {
          super(url, options);
          if (!String(url).includes("/api/sync/stream")) return;
          window.workerOverviewStream = this;
          this.addEventListener("resources", (event) => {
            window.workerOverviewResource = JSON.parse(event.data);
          });
        }
      };
    });
    await page.goto(origin);
    const selectLead = () =>
      page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
    await selectLead();
    assert.equal(
      await page.locator("#team-toggle").getAttribute("aria-expanded"),
      "false",
    );
    await page.locator("#team-toggle").click();
    const team = page.getByRole("complementary", { name: "Team", exact: true });
    const summary = team.getByLabel("Team status summary");
    // The summary lists only states with members; an absent state counts zero.
    const count = async (name) => {
      await summary.waitFor();
      const value = summary.locator(`[data-team-count="${name}"] dd`);
      return (await value.count()) ? Number(await value.innerText()) : 0;
    };
    const card = (n) =>
      team
        .locator(".worker-entry")
        .filter({ has: page.locator(`[data-worker="${worker(n).id}"]`) });
    const refreshFromEntityPull = async () => {
      fixtureRefreshStage++;
      await page.evaluate(() => {
        const source = window.workerOverviewStream;
        const current = window.workerOverviewResource;
        if (!source || !current)
          throw new Error("The real sync resource stream is not connected");
        const next = {
          ...current,
          reason: "change",
          revision: current.revision + 1,
        };
        window.workerOverviewResource = next;
        source.dispatchEvent(
          new MessageEvent("resources", { data: JSON.stringify(next) }),
        );
      });
    };
    assert.equal(
      await page.locator("#conversation-title").innerText(),
      "Release lead",
    );
    assert.equal(
      await card(7).locator(".worker-error").innerText(),
      workerFailure.message,
    );
    assert.equal(await card(24).locator(".worker-error").count(), 0);
    assert.equal(
      await card(24).locator(".worker-stop-reason").innerText(),
      stopReason,
    );
    assert.equal(
      await card(24).locator(".worker-stop-details summary").innerText(),
      "Stop details",
    );
    assert.notEqual(
      await card(24)
        .locator(".worker-stop-reason")
        .evaluate((node) => getComputedStyle(node).color),
      await card(7)
        .locator(".worker-error")
        .evaluate((node) => getComputedStyle(node).color),
      "An explicit stop has a neutral color",
    );
    await card(24).locator(".worker-stop-details summary").click();
    assert.equal(
      await card(24).locator(".worker-stop-details pre").innerText(),
      stopReason,
    );
    workerStopError = workerFailure;
    await refreshFromEntityPull();
    await page.waitForFunction(
      ({ id, message }) =>
        document.querySelector(`[data-worker="${id}"] .worker-error`)
          ?.textContent === message,
      { id: worker(24).id, message: workerFailure.message },
    );
    assert.equal(await card(24).locator(".worker-stop-reason").count(), 0);
    assert.equal(
      await card(24).locator(".worker-error-details summary").innerText(),
      "Error details",
    );
    workerStopError = stopReason;
    const errorDetails = card(7).locator(".worker-error-details");
    await errorDetails.locator("summary").click();
    assert.deepEqual(
      JSON.parse(await errorDetails.locator("pre").innerText()),
      workerFailure,
      "Structured worker errors retain all native details without breaking the shell",
    );
    workerFailure = {
      ...workerFailure,
      message: "The next account check still reports a usage limit.",
      additionalDetails: "Updated recovery details.",
    };
    await refreshFromEntityPull();
    await page.waitForFunction(
      ({ id, message }) =>
        document.querySelector(`[data-worker="${id}"] .worker-error`)
          ?.textContent === message,
      { id: worker(7).id, message: workerFailure.message },
    );
    assert.deepEqual(
      JSON.parse(await errorDetails.locator("pre").innerText()),
      workerFailure,
      "A later snapshot safely updates the structured error and its details",
    );
    assert.equal(
      await page.locator("#conversation-title").innerText(),
      "Release lead",
    );
    assert.equal(
      (await page.locator("[data-chat]").count()) > 0,
      true,
      "Chat navigation survives error updates",
    );
    assert.equal(await count("working"), 6);
    assert.equal(await count("answer"), 1);
    assert.equal(await count("completed"), 15);
    assert.equal(await summary.locator(".team-headline").count(), 0);
    assert.equal(await count("waiting"), 16);
    assert.equal(await count("stopped"), 1);
    assert.equal(await count("attention"), 1);
    assert.deepEqual(await summary.locator("dt").allInnerTexts(), [
      "Need you",
      "Failed",
      "Working",
      "Waiting",
      "Stopped",
      "Finished",
    ]);
    assert.deepEqual(
      await team
        .locator("#workers .team-status-group")
        .evaluateAll((groups) =>
          groups.map((group) => group.getAttribute("aria-label")),
        ),
      ["Working", "Need you", "Failed", "Waiting", "Stopped"],
      "grouped panel puts Working first and keeps Need you visible",
    );
    for (const name of ["Need you", "Failed"])
      assert.equal(
        await team
          .getByRole("region", { name, exact: true })
          .locator("[data-worker]")
          .count(),
        1,
      );
    assert.match(await card(0).innerText(), /Needs your answer/);
    assert.equal(
      await card(1).locator(".worker-model-summary").innerText(),
      "Astra 6 · High",
    );
    assert.equal(await card(1).locator(".worker-provider svg").count(), 1);
    assert.equal(
      await card(7).locator(".worker-state").getAttribute("aria-label"),
      "Failed",
    );
    assert.equal(
      await card(7).locator(".worker-state").getAttribute("title"),
      "Failed",
    );
    assert.equal(await card(7).locator(".worker-state svg").count(), 1);
    assert.equal(await card(7).locator(".chat-status-error svg").count(), 1);
    // The orchestrator sidebar does not repeat a worker's task text.
    assert.equal(await card(1).getByText(task).count(), 0);
    assert.doesNotMatch(await card(1).innerText(), /Task details unavailable/);
    const search = team.getByRole("searchbox", { name: "Find a subagent" });
    await search.fill("Verified receipt recovery");
    await card(25).waitFor({ state: "visible" });
    assert.equal(
      await team.locator("[data-worker]").count(),
      1,
      "reports are searchable",
    );
    const result = card(25)
      .locator(".worker-excerpt")
      .filter({ hasText: "Last report" });
    assert.equal(
      await result.locator(".worker-excerpt-preview").innerText(),
      report,
    );
    const resultLink = card(25).getByRole("link", {
      name: "Open submitted result",
    });
    assert.equal(await resultLink.getAttribute("href"), resultPath);
    await resultLink.click();
    await page.getByRole("heading", { name: "Submitted result" }).waitFor();
    await page.keyboard.press("Escape");
    await result.locator("summary").click();
    assert.equal(
      await result.locator(".worker-excerpt-full p").innerText(),
      report,
    );
    await result.getByRole("button", { name: "Continue in chat" }).click();
    assert.equal(
      await page.locator("#conversation-title").innerText(),
      worker(25).name,
    );
    assert.equal(await page.getByLabel("Agent progress").count(), 0);
    assert.equal(
      await page
        .getByText("This worker progress file must not appear in its chat.")
        .count(),
      0,
    );
    await page
      .getByRole("button", { name: "Back to main agent", exact: true })
      .click();
    await search.fill("");
    deferred = true;
    await page.reload();
    await selectLead();
    assert.equal(
      await count("answer"),
      0,
      "deferred questions do not remind the user",
    );
    assert.equal(
      await count("working"),
      6,
      "a blocked deferred question is not working",
    );
    assert.match(await card(0).innerText(), /Question deferred/);
    pending = false;
    deferred = false;
    await page.reload();
    await selectLead();
    assert.equal(await count("answer"), 0, "answered requests stop counting");
    assert.equal(await count("working"), 7);
    await page.screenshot({ path: join(root, "worker-overview-desktop.png") });
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    assert.equal(
      await team.count(),
      0,
      "another chat cannot show the team summary",
    );
    await selectLead();
    await page.setViewportSize({ width: 390, height: 844 });
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Team", exact: true }).click();
    await search.waitFor({ state: "visible" });
    await search.fill("exact response receipt");
    await card(1).waitFor({ state: "visible" });
    await card(1).locator(".worker-excerpt summary").click();
    assert.equal(
      await card(1).evaluate((node) => node.scrollWidth <= node.clientWidth),
      true,
    );
    assert.equal(await card(1).locator("strong").innerText(), longName);
    await page.screenshot({ path: join(root, "worker-overview-mobile.png") });
    await card(1)
      .getByRole("button", {
        name: `Options for subagent ${longName}`,
        exact: true,
      })
      .click();
    await page.getByRole("menuitem", { name: "Delete", exact: true }).click();
    const [deletedResponse] = await Promise.all([
      page.waitForResponse(
        (response) =>
          response.url().endsWith("/api/conversation/delete") &&
          response.request().method() === "POST",
      ),
      page.locator(`[data-delete-chat="${worker(1).id}"]`).click(),
    ]);
    const receipt = await deletedResponse.json();
    assert.deepEqual(receipt.deleted, [worker(1).id]);
    await card(1).waitFor({ state: "hidden" });
    const remaining = await readTestState(origin);
    assert.ok(remaining.threads.some((agent) => agent.id === lead.id));
    assert.ok(remaining.threads.some((agent) => agent.id === worker(2).id));
    assert.ok(!remaining.threads.some((agent) => agent.id === worker(1).id));
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        passed: true,
        summary: true,
        actualExcerpts: true,
        disclosure: true,
        noTranscriptFanout: true,
        search: true,
        chatIsolation: true,
        narrowLayout: true,
        screenshots: root,
      }),
    );
  } catch (error) {
    await page?.screenshot({ path: join(root, "failure.png") });
    console.error("Failure evidence:", root);
    throw error;
  } finally {
  }
});
