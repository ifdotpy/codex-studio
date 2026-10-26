import type { components } from "../../src/generated/api.ts";
import { required } from "./fixture-helpers.ts";
import type { DaybreakReset } from "./daybreak-settings-browser.fixture.tsx";
import type {} from "./daybreak-settings-browser.fixture.tsx";
import { fixtureUrl } from "./fixture-helpers.ts";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  modelOptionDisabled,
  modelOptions,
  modelValue,
  selectModel,
} from "../fixtures/model-picker.ts";
import { test } from "../playwright.ts";
import { setupControl, setupToggle } from "../fixtures/setup-controls.ts";

test("Daybreak settings", async ({ context }) => {
  test.setTimeout(60_000);
  // Exercise the real settings component without a backend or model request.

  const root = fileURLToPath(new URL("../../", import.meta.url));

  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-daybreak-settings-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
        "@mantine/core",
        "lucide-react",
        "@mantine/hooks",
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
    plugins: [
      {
        name: "daybreak-settings-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/settings/daybreak-settings-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });

  try {
    await server.listen();
    const page = await context.newPage();
    await page.setViewportSize({ width: 390, height: 700 });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/sync/stream?**", (route) => {
      const resources = JSON.parse(
        new URL(route.request().url()).searchParams.get("resources") || "[]",
      );
      return route.fulfill({
        contentType: "text/event-stream",
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
        body: apiSchemaHandshakeSse(
          `event: resources\ndata: ${JSON.stringify({ protocol: 4, workspaceId: "1234567890abcdef1234567890abcdef", epoch: "fixture", revision: 1, reason: "initial", resources })}\n\n`,
        ),
      });
    });
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
        json: {
          workspaceId: "1234567890abcdef1234567890abcdef",
          syncProtocol: 2,
        } satisfies components["schemas"]["SyncIdentityResponse"],
      }),
    );
    await page.goto(fixtureUrl(server, "check"));
    await page.waitForFunction(() => window.daybreakFixture?.ready, undefined, {
      timeout: 5_000,
    });
    const reset = async (options?: DaybreakReset) => {
      await page.evaluate((options) => {
        localStorage.clear();
        window.daybreakFixture.reset(options);
      }, options);
      await page
        .getByRole("button", {
          name: options?.defaults ? "Subagent defaults" : "Main agent settings",
          exact: true,
        })
        .click();
    };
    const mode = () =>
      setupToggle(page.getByRole("switch", { name: "Daybreak", exact: true }));
    const model = (defaults = false) =>
      page.getByLabel(
        defaults ? "Default subagent model" : "Main agent model",
        {
          exact: true,
        },
      );
    const settled = async () =>
      page.waitForFunction(
        () =>
          window.daybreakFixture.refreshCount ===
            window.daybreakFixture.calls.length &&
          window.daybreakFixture.calls.length > 0,
      );
    const clearActivity = () =>
      page.evaluate(() => {
        window.daybreakFixture.calls = [];
        window.daybreakFixture.refreshCount = 0;
      });

    const enable = async (defaults = false) => {
      const activeModel = model(defaults);
      await activeModel.waitFor();
      if (!(await mode().count())) {
        await selectModel(activeModel, "gpt-5.6-sol");
        await settled();
        await clearActivity();
      }
      await mode().check();
    };

    await reset();
    assert.equal(
      (await modelOptions(model())).includes("gpt-daybreak-blue-latest")
        ? 1
        : 0,
      0,
    );
    assert.equal(
      (await modelOptions(model())).includes("gpt-daybreak-red-latest") ? 1 : 0,
      0,
    );
    assert.equal(await modelOptionDisabled(model(), "daybreak-only"), true);
    await enable();
    await settled();
    assert.deepEqual(
      await page.evaluate(() => window.daybreakFixture.calls[0]),
      {
        id: "first",
        expected_account_key: "default",
        model: "gpt-5.6-sol",
        effort: "high",
        fast_mode: true,
        daybreak_enabled: true,
      },
    );
    assert.equal(await modelValue(model()), "gpt-5.6-sol");
    assert.equal(await mode().isChecked(), true);
    assert.equal(await modelOptionDisabled(model(), "gpt-6-astra"), true);
    assert.equal(await modelOptionDisabled(model(), "gpt-5.6-luna"), true);
    assert.equal(await modelOptionDisabled(model(), "gpt-5.6-terra"), false);
    assert.match(
      await page
        .getByRole("button", { name: "Main agent settings", exact: true })
        .innerText(),
      /Sol/,
    );
    if (process.env.SCREENSHOT)
      await page.screenshot({ path: process.env.SCREENSHOT });
    // A stale replication snapshot must not erase the acknowledged mode.
    await page.evaluate(() =>
      window.daybreakFixture.setAgent({
        ...window.daybreakFixture.agent,
        tail: "Unrelated update",
      }),
    );
    assert.equal(await mode().isChecked(), true);
    await page.evaluate(() =>
      window.daybreakFixture.setAgent({
        ...window.daybreakFixture.agent,
        model: "gpt-5.6-sol",
        daybreakEnabled: true,
      }),
    );
    await page.evaluate(
      () => new Promise((resolve) => requestAnimationFrame(resolve)),
    );
    await page.evaluate(() =>
      window.daybreakFixture.setAgent({
        ...window.daybreakFixture.agent,
        daybreakEnabled: false,
      }),
    );
    assert.equal(await mode().isChecked(), false);
    console.log(
      "PASS separate mode, alias exclusion, capability gating, fallback model, effort preservation, canonical replication",
    );

    // Explicit daybreak-only rows cannot be selected when standard mode is active.
    await reset({
      agent: {
        model: "daybreak-only",
        daybreakEnabled: true,
        effort: "low",
        fastMode: false,
      },
    });
    await mode().uncheck();
    await settled();
    assert.equal(await modelValue(model()), "gpt-5.6-sol");
    assert.equal(
      await page.evaluate(
        () =>
          window.daybreakFixture.required(window.daybreakFixture.calls[0])
            .daybreak_enabled,
      ),
      false,
    );
    await reset({ agent: { model: "gpt-5.6-terra" } });
    // Terra supports Red mode. Wait for that setting to render before toggling:
    // the generic enable helper can select the default model if called too early.
    await page.getByRole("switch", { name: "Daybreak", exact: true }).waitFor();
    await mode().check();
    await settled();
    assert.equal(
      await modelValue(model()),
      "gpt-5.6-terra",
      "Red-only support keeps the selected model",
    );
    await clearActivity();
    await selectModel(model(), "daybreak-only");
    await settled();
    assert.equal(
      await page
        .getByLabel("Main agent reasoning", { exact: true })
        .getAttribute("data-value"),
      "low",
    );
    assert.equal(
      await page
        .getByRole("switch", { name: "Fast mode", exact: true })
        .count(),
      0,
    );
    console.log(
      "PASS standard fallback, Red support, model-dependent effort and Fast reset",
    );

    await reset({
      models: [
        {
          model: "gpt-6-astra",
          serviceTiers: [],
          supportedReasoningEfforts: [{ reasoningEffort: "high" }],
        },
      ],
    });
    assert.equal(await mode().count(), 0);
    assert.equal(await modelOptionDisabled(model(), "gpt-6-astra"), false);
    assert.deepEqual(
      await page.evaluate(() => window.daybreakFixture.calls),
      [],
    );
    await reset({ agent: { model: "gpt-daybreak-blue-latest" } });
    assert.equal(
      await modelOptionDisabled(model(), "gpt-daybreak-blue-latest"),
      true,
    );
    assert.equal(
      await setupControl(
        page.getByLabel("Main agent reasoning", { exact: true }),
      ).isDisabled(),
      true,
    );
    await enable();
    await settled();
    assert.equal(await modelValue(model()), "gpt-5.6-sol");
    console.log(
      "PASS missing metadata hides mode, legacy alias remains read-only without invented mapping",
    );

    await reset({ agent: { status: "running" } });
    await page.getByRole("switch", { name: "Daybreak", exact: true }).waitFor();
    await page.evaluate(() => (window.daybreakFixture.fail = "network"));
    await mode().check();
    await page
      .getByRole("button", { name: "Check settings save", exact: true })
      .waitFor();
    const request = await page.evaluate(() => window.daybreakFixture.calls[0]);
    assert.equal(required(request).next_turn, true);
    assert.equal(required(request).daybreak_enabled, true);
    assert.equal(
      await page.evaluate(() => window.daybreakFixture.agent.daybreakEnabled),
      false,
    );
    assert.match(await page.locator("body").innerText(), /next turn/i);
    await page.reload();
    await page.waitForFunction(() => window.daybreakFixture?.ready, undefined, {
      timeout: 5_000,
    });
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Check settings save", exact: true })
      .click();
    await settled();
    assert.deepEqual(
      await page.evaluate(() => window.daybreakFixture.calls[0]),
      request,
      "Retry retains the exact mode and request identity",
    );
    assert.equal(await mode().isChecked(), true);
    console.log(
      "PASS queued Daybreak, current turn unchanged, lost response reload and exact receipt replay",
    );

    await reset({ agent: { status: "running" } });
    await page.getByRole("switch", { name: "Daybreak", exact: true }).waitFor();
    await page.evaluate(() => (window.daybreakFixture.fail = "network"));
    await mode().check();
    await page
      .getByRole("button", { name: "Check settings save", exact: true })
      .waitFor();
    await page.evaluate(() =>
      window.daybreakFixture.setAgent({
        ...window.daybreakFixture.agent,
        accountKey: "second",
        model: "gpt-5.6-terra",
        daybreakEnabled: false,
      }),
    );
    const changedAccountTrigger = page.getByRole("button", {
      name: "Main agent settings",
      exact: true,
    });
    if ((await changedAccountTrigger.getAttribute("aria-expanded")) !== "true")
      await changedAccountTrigger.click();
    assert.equal(await mode().isChecked(), false);
    assert.equal(
      await page
        .getByRole("button", { name: "Check settings save", exact: true })
        .count(),
      0,
    );
    await reset({
      agent: {
        pendingSettingsAccountKey: "old-account",
        pendingSettings: { model: "gpt-5.6-sol", daybreakEnabled: true },
      },
    });
    assert.equal(await mode().isChecked(), false);
    await page.evaluate(() => (window.daybreakFixture.fail = "rejected"));
    await enable();
    await page
      .getByRole("alert")
      .filter({ hasText: "Mode not supported" })
      .waitFor();
    assert.equal(await mode().isChecked(), false);
    assert.equal(await modelValue(model()), "gpt-5.6-sol");
    console.log(
      "PASS account-scoped mode receipts, stale account settings ignored, rejected mode rollback",
    );

    await reset({
      defaults: true,
      agent: {
        model: "gpt-6-astra",
        workerDefaults: {
          model: null,
          effort: "high",
          fastMode: false,
          daybreakEnabled: false,
        },
      },
    });
    await enable(true);
    await settled();
    assert.deepEqual(
      await page.evaluate(() => window.daybreakFixture.calls[0]),
      {
        id: "first",
        expected_account_key: "default",
        worker_defaults: {
          account_key: null,
          model: "gpt-5.6-sol",
          effort: "high",
          fast_mode: false,
          daybreak_enabled: true,
          delegation_depth: 1,
        },
      },
    );
    assert.equal(
      await modelOptionDisabled(model(true), "__model_default__"),
      true,
    );
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth + 1,
      ),
      true,
    );
    await reset({ agent: { provider: "claude" } });
    assert.equal(await mode().count(), 0);
    console.log(
      "PASS subagent defaults, inherited model capability gate, narrow layout, Claude mode hidden",
    );
    assert.deepEqual(errors, []);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
