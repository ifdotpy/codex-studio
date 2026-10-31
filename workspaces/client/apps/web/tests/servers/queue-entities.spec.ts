import { required } from "../teams/helpers.ts";
import assert from "node:assert/strict";
import type { components } from "../../src/generated/api.ts";
import { fixtureOrigin } from "./browser-fixture.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
  readCachedRows,
  remoteOrigin,
  REMOTE_FIXTURE_HOST,
} from "../playwright.ts";
import { fixture } from "./multi-server-fixture.ts";

test("bounded entities render initial and live changes with zero resource GETs", async ({
  page,
}) => {
  const state = await mkdtemp(join(tmpdir(), "studio-queue-entities-"));
  const backend = await fixture("Queue", false, { entityFrames: true });
  const queue = (
    text: string,
    revision: string,
  ): components["schemas"]["QueueView"] => ({
    items: [
      {
        id: "queued",
        kind: "user",
        status: "pending",
        created: 1,
        text,
        delivery: "next_turn",
        requestedDelivery: "next_turn",
        assets: [],
      },
    ],
    revision,
    capabilities: { reorder: true, receipts: true },
  });
  backend.snapshot.runtime.queuesByAgent = {
    overlap: queue("Initial stream queue", "first"),
  };
  backend.snapshot.runtime.accountCostsByAccount = {
    default: {
      accountKey: "default",
      at: 1,
      error: null,
      data: null,
      refreshing: false,
      stale: false,
    },
  };
  backend.snapshot.runtime.sessionCostsByAgent = {
    overlap: {
      rootId: "overlap",
      totalUSD: 0.5,
      pricedSamples: 1,
      breakdown: { providers: {}, models: {} },
      unknownModels: [],
      estimated: true,
      pricingState: "ready",
      cacheAgeSeconds: 0,
      refreshing: false,
    },
  };
  backend.snapshot.runtime.desktop = {
    application: "codex-agents",
    protocol: 1,
    mobileProtocol: 1,
    backendBuild: "fixture",
    nativeRuntime: {
      status: "checking",
      checkedAt: null,
      selected: null,
      candidates: [],
      accounts: {},
    },
    providerVersions: { checkedAt: null, providers: [], warnings: [] },
    browser: { enabled: false, reason: "Fixture" },
    liveUpdate: null,
    restartEnvironment: {},
    publicOrigin: null,
    pid: 1,
    supervisorMode: false,
    supervisorRequired: false,
    supervisorError: null,
    supervisor: null,
    supervisorFallback: false,
    stateDir: "/same/state",
    supervisorNotice: "Initial desktop stream",
    diskSpace: { level: null, message: null },
  };
  backend.snapshot.runtime.terminals = [
    {
      id: "shell",
      agent: "overlap",
      title: "Initial shell",
      cwd: "/fixture",
      status: "running",
      created: 1,
    },
  ];
  const vite = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    cacheDir: join(state, "vite"),
    server: {
      host: "127.0.0.1",
      port: 0,
      allowedHosts: [REMOTE_FIXTURE_HOST],
      proxy: { "/api": backend.origin },
    },
  });
  const reads: string[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "GET" &&
      [
        "/api/queue",
        "/api/accounts",
        "/api/costs",
        "/api/session-cost",
        "/api/desktop",
        "/api/terminals",
      ].includes(new URL(request.url()).pathname)
    )
      reads.push(request.url());
  });
  try {
    await vite.listen();
    // A remote host name: only a remote frontend keeps the thin cache.
    await page.goto(remoteOrigin(fixtureOrigin(vite)));
    await expect(
      page.getByText("Initial stream queue", { exact: true }),
    ).toBeVisible({ timeout: 20_000 });
    await expect(
      page.getByText("Initial desktop stream", { exact: true }),
    ).toBeVisible();
    await page.waitForTimeout(1100);
    expect(reads).toHaveLength(0);
    test.info().annotations.push({
      type: "resource-GETs-before-update",
      description: String(reads.length),
    });

    assert.ok(backend.snapshot.runtime.accountsSummary?.accounts[0]);
    assert.ok(backend.snapshot.runtime.accountCostsByAccount.default);
    assert.ok(backend.snapshot.runtime.sessionCostsByAgent.overlap);
    assert.ok(backend.snapshot.runtime.terminals[0]);
    backend.snapshot.runtime.queuesByAgent.overlap = queue(
      "Updated stream queue",
      "second",
    );
    backend.snapshot.runtime.desktop.supervisorNotice =
      "Updated desktop stream";
    backend.snapshot.runtime.accountsSummary!.accounts[0].label =
      "Updated account";
    backend.snapshot.runtime.accountCostsByAccount.default.at = 2;
    backend.snapshot.runtime.sessionCostsByAgent.overlap.totalUSD = 0.75;
    backend.snapshot.runtime.terminals[0].title = "Updated shell";
    backend.publishState();
    await expect(
      page.getByText("Updated stream queue", { exact: true }),
    ).toBeVisible();
    await expect(
      page.getByText("Updated desktop stream", { exact: true }),
    ).toBeVisible();
    // Read the cache record of the mounted server scope. A new default sync
    // client would create a second subscription with a separate service owner.
    const readProjection = async () => {
      const rows = await readCachedRows(page, "entities");
      return page.evaluate(async (rows) => {
        const { applyEntityRows, emptyEntityProjection } =
          await import("/src/sync/entityProjection.ts");
        return applyEntityRows(emptyEntityProjection(), rows, true).runtime;
      }, rows);
    };
    // The record is written after render; poll until it holds the update.
    await expect
      .poll(async () => (await readProjection()).accountCostsByAccount?.default)
      .toBeDefined();
    const projection = await readProjection();
    expect(projection.accountCostsByAccount).toEqual({
      default: backend.snapshot.runtime.accountCostsByAccount.default,
    });
    expect(required(projection.accountsSummary!.accounts[0]).label).toBe(
      "Updated account",
    );
    expect(required(projection.sessionCostsByAgent!.overlap).totalUSD).toBe(
      0.75,
    );
    expect(required(projection.terminals![0]).title).toBe("Updated shell");
    await page.waitForTimeout(1100);
    expect(reads).toHaveLength(0);
    test.info().annotations.push({
      type: "resource-GETs-after-update",
      description: String(reads.length),
    });
  } finally {
    await page.close();
    await vite.close();
    await backend.close();
    await rm(state, { recursive: true, force: true });
  }
});
