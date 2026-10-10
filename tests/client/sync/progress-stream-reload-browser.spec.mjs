import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { readTestState, expect, spawnFixture, test } from "../playwright.mjs";

test("active panel resource stream reconnects promptly after repeated reloads", async ({
  page,
}) => {
  test.setTimeout(120_000);
  test.fixme(
    true,
    "Intermittent product race: the expected failure occurred in 10/10 focused ordinary repeats but unexpectedly passed in two ordinary full-suite runs; see KNOWN-FAILURES.md.",
  );
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "studio-progress-reload-"));
  const fixture = spawnFixture(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, PROGRESS_PUSH_UI_FIXTURE: "1" },
    },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(fixtureLog)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const streamResponses = [];
  const requestGenerations = new WeakMap();
  let activeGeneration = 0;
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/api/sync/stream") {
      requestGenerations.set(request, {
        generation: activeGeneration,
        startedAt: Date.now(),
      });
    }
  });
  page.on("response", (response) => {
    const url = new URL(response.url());
    if (url.pathname !== "/api/sync/stream") return;
    const request = requestGenerations.get(response.request());
    const resources = JSON.parse(url.searchParams.get("resources") || "[]");
    streamResponses.push({
      generation: request?.generation ?? -1,
      requestStartedAt: request?.startedAt ?? null,
      respondedAt: Date.now(),
      resources,
      panelAgentIds: resources
        .filter((resource) => resource.kind === "panel")
        .map((resource) => resource.agentId),
      status: response.status(),
    });
  });
  const progressWatchState = async () =>
    fetch(`${origin}/api/test/progress-watch-state`).then((response) =>
      response.json(),
    );
  try {
    await readTestState(origin);
    await page.goto(origin);
    await page.locator("[data-chat]").first().waitFor();
    await expect
      .poll(async () => (await progressWatchState()).activeAgentIds.length)
      .toBeGreaterThan(0);
    await expect
      .poll(() =>
        streamResponses.some(
          (response) =>
            response.status === 200 && response.panelAgentIds.length,
        ),
      )
      .toBe(true);
    const firstPanelOpen = streamResponses.find(
      (response) => response.status === 200 && response.panelAgentIds.length,
    );
    const panelAgentId = firstPanelOpen.panelAgentIds[0];
    await expect
      .poll(async () =>
        (await progressWatchState()).activeAgentIds.includes(panelAgentId),
      )
      .toBe(true);

    const reloads = [];
    for (let index = 0; index < 30; index++) {
      const generation = index + 1;
      activeGeneration = generation;
      const startedAt = Date.now();
      await page.reload({ waitUntil: "domcontentloaded" });
      await page.locator("[data-chat]").first().waitFor();
      await expect
        .poll(
          () =>
            streamResponses.some(
              (response) =>
                response.generation === generation &&
                response.status === 200 &&
                response.panelAgentIds.includes(panelAgentId),
            ),
          { timeout: 20_000 },
        )
        .toBe(true);
      await expect
        .poll(async () =>
          (await progressWatchState()).activeAgentIds.includes(panelAgentId),
        )
        .toBe(true);
      const success = streamResponses.find(
        (response) =>
          response.generation === generation &&
          response.status === 200 &&
          response.panelAgentIds.includes(panelAgentId),
      );
      reloads.push({
        index: generation,
        elapsedMs: success.respondedAt - startedAt,
        failedOpens: 0,
      });
    }
    // Let any requests waiting on the server's five-second detach guard return
    // so each failure is attributed to the navigation that made it.
    await new Promise((resolve) => setTimeout(resolve, 5_500));
    const failures = streamResponses
      .filter((response) => response.status >= 400)
      .map(
        ({
          generation,
          status,
          requestStartedAt,
          respondedAt,
          resources,
          panelAgentIds,
        }) => ({
          generation,
          status,
          requestDurationMs: respondedAt - requestStartedAt,
          panelAgentIds,
          resourceKinds: resources.map(({ kind }) => kind),
        }),
      );
    for (const reload of reloads) {
      reload.failedOpens = failures.filter(
        (failure) => failure.generation === reload.index,
      ).length;
    }

    console.log(`RESOURCE_STREAM_RELOADS ${JSON.stringify(reloads)}`);
    console.log(`RESOURCE_STREAM_FAILURES ${JSON.stringify(failures)}`);
    expect(
      reloads.filter(
        (reload) => reload.failedOpens || reload.elapsedMs > 2_000,
      ),
    ).toEqual([]);
  } finally {
    await page.unrouteAll({ behavior: "wait" });
    fixture.kill("SIGTERM");
  }
});
