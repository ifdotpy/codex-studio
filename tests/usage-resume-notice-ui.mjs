#!/usr/bin/env node
// Render the production limit notice and verify its schedule and opt-out action.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "usage-resume-notice-vite-"));
const entry = join(repo, "web/__usage-resume-fixture.jsx");
const dueAt = Math.floor(Date.now() / 1000) + 3600;
const source = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import {NativeError} from '/src/components/NativeNotice.tsx';
const resume={id:'resume-1',status:'scheduled',dueAt:${dueAt},plannedAt:${dueAt},updatedAt:1};
const agent={id:'chat-1',rootId:'chat-1',accountKey:'work',threadId:'thread-1',model:'gpt-6-astra',
  error:{codexErrorInfo:'usageLimitExceeded',message:'Usage limit reached'},nativeLimitErrorAt:${dueAt - 60},
  status:'failed',isLead:true,usageResume:resume,usageResumeEnabled:true};
createRoot(document.getElementById('root')).render(<MantineProvider><NativeError agent={agent} limits={{
  accountKey:'work',at:${dueAt - 30},data:{rateLimits:{primary:{usedPercent:100,resetsAt:${dueAt}}}}
}}/></MantineProvider>);`;
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: join(repo, "web"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "usage-resume-fixture",
      resolveId(id) {
        if (id === "/__usage-resume-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__usage-resume-fixture.jsx"></script>',
          );
        });
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage();
  const writes = [];
  page.on("pageerror", (error) => writes.push(`pageerror:${error.message}`));
  await page.route("**/api/usage-resume", async (route) => {
    const body = route.request().postDataJSON();
    writes.push(body);
    await route.fulfill({
      json: {
        id: "resume-1",
        status: "cancelled",
        dueAt: null,
        reason: "Automatic resume is off for this chat.",
        updatedAt: 2,
      },
    });
  });
  await page.goto(server.resolvedUrls.local[0]);
  const notice = page.locator(".account-limit-recovery.inline");
  await notice.waitFor();
  assert.match(await notice.textContent(), /Automatic resume planned for/);
  assert.ok(
    (await notice
      .locator(`time[datetime="${new Date(dueAt * 1000).toISOString()}"]`)
      .count()) >= 1,
  );
  await notice
    .getByRole("button", { name: "Turn off automatic resume" })
    .click();
  await notice
    .getByRole("button", { name: "Turn on automatic resume" })
    .waitFor();
  assert.deepEqual(writes, [
    { id: "chat-1", resume_id: "resume-1", enabled: false },
  ]);
  console.log("Usage resume notice: planned time and opt-out toggle passed.");
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
