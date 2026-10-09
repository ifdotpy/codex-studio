import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
  test,
  expect,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("subagent concurrency uses the confirmed response", async ({ page }) => {
  test.setTimeout(60000);
  const root = join(import.meta.dirname, "../../");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(
    join(tmpdir(), "studio-concurrency-confirmed-"),
  );
  const entry = join(root, "concurrency-confirmed-fixture.tsx");
  const workspaceId = "1234567890abcdef1234567890abcdef";
  const initial = {
    id: "lead",
    accountKey: "default",
    name: "Lead",
    isLead: true,
    status: "idle",
    subagentConcurrencyVersion: 2,
    concurrency: 32,
    agentModeRevision: 0,
  };
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "concurrency-confirmed-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/concurrency-confirmed-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/concurrency-confirmed-fixture.tsx") return entry;
        },
        load(id) {
          if (
            process.env.CONFIRMATION_BASELINE &&
            id ===
              join(root, "src/components/agents/SubagentConcurrencyControl.tsx")
          )
            return execFileSync(
              "git",
              [
                "show",
                `${process.env.CONFIRMATION_BASELINE}:web/src/components/agents/SubagentConcurrencyControl.tsx`,
              ],
              { cwd: root, encoding: "utf8" },
            );
          if (id !== entry) return;
          return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {flushSync} from 'react-dom';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import Control from '/src/components/agents/SubagentConcurrencyControl.tsx';
            window.refreshCount=0;window.refreshHold=true;window.refreshFailure=false;
            function Fixture(){const[lead,setLead]=useState(${JSON.stringify(initial)});window.setLead=value=>flushSync(()=>setLead(value));return <MantineProvider><Control compact lead={lead} stateDir="fixture-state" workspaceId="${workspaceId}" refresh={async()=>{window.refreshCount++;if(window.refreshHold)await new Promise(resolve=>window.refreshRelease=resolve);if(window.refreshFailure)throw new Error('Credentials unavailable')}}/></MantineProvider>}createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let releaseReply;
  const writes = [];
  let reply = { ...initial, concurrency: 64, agentModeRevision: 1 };
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      json: { workspaceId, syncProtocol: 2 },
    }),
  );
  await page.route("**/api/sync/stream?**", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: apiSchemaHandshakeSse(),
    }),
  );
  await page.route("**/api/conversation", async (route) => {
    writes.push(route.request().postDataJSON());
    await new Promise((resolve) => (releaseReply = resolve));
    await route.fulfill({
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      json: reply,
    });
  });
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const control = page.locator(".agent-mode-control");
    const input = page.getByRole("spinbutton", {
      name: "Subagent parallelism",
    });
    await expect(control).toHaveAttribute("data-concurrency", "32");
    await input.fill("64");
    await input.blur();
    await expect.poll(() => writes.length).toBe(1);
    assert.equal(writes[0].expected_mode_revision, 0);
    await expect(control).toHaveAttribute("data-concurrency", "32");
    await expect(input).toBeDisabled();
    assert.equal(await page.evaluate(() => window.refreshCount), 0);
    releaseReply();
    await page.waitForFunction(() => !!window.refreshRelease);
    await expect(control).toHaveAttribute("data-concurrency", "64");
    await expect(control).toHaveAttribute("data-agent-mode-revision", "1");
    await expect(input).toBeEnabled({ timeout: 1000 });
    assert.equal(writes[0].expected_account_key, "default");
    await expect(control.getByText("Applying…", { exact: true })).toHaveCount(
      0,
    );
    await page.evaluate(() => {
      window.refreshFailure = true;
      window.refreshRelease();
    });
    await expect(control.getByRole("alert")).toContainText(
      "Setting saved. Credentials unavailable",
    );
    await expect(control).toHaveAttribute("data-concurrency", "64");
    await expect(input).toBeEnabled();
    await page.evaluate(() => {
      window.refreshHold = false;
      window.refreshFailure = false;
    });
    reply = { ...initial, concurrency: 0, agentModeRevision: 2 };
    await input.fill("0");
    await input.blur();
    await expect.poll(() => writes.length).toBe(2);
    await expect(control).toHaveAttribute("data-agent-mode", "multi");
    await expect(input).toBeDisabled();
    releaseReply();
    await expect(control).toHaveAttribute("data-agent-mode", "single");
    await expect(input).toBeEnabled();
    assert.equal(writes[1].expected_mode_revision, 1);
    // A newer snapshot remains authoritative when an older response arrives.
    reply = { ...initial, concurrency: 128, agentModeRevision: 3 };
    await input.fill("128");
    await input.blur();
    await expect.poll(() => writes.length).toBe(3);
    await page.evaluate((lead) => window.setLead(lead), {
      ...initial,
      concurrency: 8,
      agentModeRevision: 4,
    });
    releaseReply();
    await expect(input).toBeEnabled();
    await expect(control).toHaveAttribute("data-concurrency", "8");
    await expect(control).toHaveAttribute("data-agent-mode-revision", "4");
    await expect(input).toHaveValue("8");
    assert.deepEqual(errors, []);
  } finally {
    releaseReply?.();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
