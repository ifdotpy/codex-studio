// Exercise the Codex 0.159 write_stdin_approval request through the real card and answer API.
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.mjs";

test("Terminal Input Approval Ui", async ({
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

  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-stdin-approval-"));
  const harness = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import Requests from '/src/components/questions/Requests.tsx';
const agent={id:'lead',rootId:'lead',name:'Lead',epoch:0,accountKey:'default',turnId:'turn',inFlight:true};
const request={id:'request-159',agent:'lead',method:'item/commandExecution/requestApproval',params:{kind:'writeStdin',threadId:'thread-159',turnId:'turn-159',itemId:'exec-159',startedAtMs:1750000000000,approvalId:'stdin-approval-159',reason:'Send input to an existing terminal to continue the reviewed command.'}};
createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,null,React.createElement(Requests,{requests:[request],allRequests:[request],scope:'fixture',agents:[agent],refresh:async()=>{},notify:()=>{}})));
`;
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: join(temporary, "vite"),
    plugins: [
      {
        name: "stdin-approval-fixture",
        resolveId(id) {
          return id === "virtual:stdin-approval" ? "\0" + id : undefined;
        },
        load(id) {
          return id === "\0virtual:stdin-approval" ? harness : undefined;
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = testPage;
    page.setDefaultTimeout(5000);
    const answers = [];
    await page.route("**/api/**", async (route) => {
      if (route.request().method() === "POST") {
        answers.push(route.request().postDataJSON());
        return route.fulfill({ json: { status: "answered" } });
      }
      return route.fulfill({ json: {} });
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__virtual:stdin-approval";</script>',
      }),
    );
    await page.goto(server.resolvedUrls.local[0] + "check");
    await page.waitForFunction(() =>
      document.querySelector('[data-request="request-159"]'),
    );
    await page
      .getByText("Terminal input approval required", { exact: true })
      .waitFor();
    await page
      .getByText(
        "Send input to an existing terminal to continue the reviewed command.",
        { exact: true },
      )
      .waitFor();
    await page
      .getByRole("button", { name: "Allow input", exact: true })
      .click();
    const deadline = Date.now() + 5000;
    while (!answers.length && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert.deepEqual(answers, [{ decision: "accept", id: "request-159" }]);
    console.log(
      "PASS: 0.159 writeStdin request renders its reason and submits one approval reply",
    );
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
