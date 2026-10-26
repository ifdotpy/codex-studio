import { browserSignInUrl } from "./sign-in.fixture.ts";
import { required } from "../messages/helpers.ts";
import type { DesktopBridge } from "../../src/desktop.ts";
import type {} from "./browser-open-approval-ui.fixture.tsx";
import { createFixtureViteServer } from "../playwright.ts";
import { requestBody } from "../messages/helpers.ts";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
// A VM agent's sign-in link (browser/open, docs/vm-browser-bridge.md) renders as a sign-in
// card. Nothing opens before the approval; after it, the desktop opens only the link that the
// server returned. A refusal from the server, such as a remote window, opens nothing.

import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";

const REMOTE =
  "Open this chat in the window of the Mac that runs its server to sign in, or use the tool's device-code mode.";

test("Browser Open Approval Ui", async ({ page }) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const server = await createFixtureViteServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    // Prepare the card dependencies before the first page load.
    optimizeDeps: {
      entries: ["src/components/questions/Requests.tsx"],
      include: ["react", "react-dom/client", "@mantine/core"],
    },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    page.setDefaultTimeout(5000);
    // The desktop bridge records what it would open; no real browser opens.
    await page.addInitScript(() => {
      window.browserApprovalFixture = { opened: [], notes: [] };
      Object.defineProperty(window, "codexDesktop", {
        value: {
          platform: "test",
          openExternal: async (url) => {
            window.browserApprovalFixture.opened.push(["external", url]);
          },
          openSignIn: async (url) => {
            window.browserApprovalFixture.opened.push(["signIn", url]);
          },
        } satisfies Pick<
          DesktopBridge,
          "platform" | "openExternal" | "openSignIn"
        >,
      });
    });
    const answers: PostBody<"/api/answer">[] = [];
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "POST")
        return route.fulfill({ json: {} });
      const body = requestBody<PostBody<"/api/answer">>(route);
      answers.push(body);
      if (body.id === "remote")
        return route.fulfill({ status: 400, json: { error: REMOTE } });
      if (body.decision === "accept")
        return route.fulfill({
          json: {
            status: "answered",
            openUrl:
              body.id === "approve" ? browserSignInUrl : "https://x.invalid/",
          } satisfies PostResult<"/api/answer">,
        });
      return route.fulfill({
        json: { status: "answered" } satisfies PostResult<"/api/answer">,
      });
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/tests/questions/browser-open-approval-ui.fixture.tsx";</script>',
      }),
    );
    // The first load waits for the Vite dependency scan, which takes longer than the
    // 5 s action limit on a busy machine.
    await page.goto(required(server.resolvedUrls?.local[0]) + "check", {
      timeout: 30_000,
    });
    const approve = page.locator('[data-request="approve"]');
    await approve.waitFor({ timeout: 30_000 });
    await expect(approve.getByText("Cluster worker")).toBeVisible();
    await expect(approve.getByText("Sign-in request")).toBeVisible();
    await expect(approve.getByText("Browser sign-in")).toBeVisible();
    await expect(
      approve.getByText("accounts.example.com", { exact: true }),
    ).toBeVisible();
    await expect(approve.getByText("kube", { exact: true })).toBeVisible();
    await expect(
      approve.getByText(
        "The VM on the chat's server, user studio-a-0123, port 8000",
      ),
    ).toBeVisible();
    // The card offers no direct link: the link opens only through the approval.
    await expect(approve.locator("a[href]")).toHaveCount(0);
    const device = page.locator('[data-request="device"]');
    await expect(device.getByText("No result to forward")).toBeVisible();
    await expect(device.getByText("Device code")).toBeVisible();
    await expect(device.getByText("ABCD", { exact: true })).toBeVisible();
    expect(
      await page.evaluate(() => window.browserApprovalFixture.opened),
    ).toEqual([]);

    await approve
      .getByRole("button", { name: "Open in browser", exact: true })
      .click();
    await expect
      .poll(() => page.evaluate(() => window.browserApprovalFixture.opened))
      .toEqual([["signIn", browserSignInUrl]]);

    const remote = page.locator('[data-request="remote"]');
    await remote
      .getByRole("button", { name: "Open in browser", exact: true })
      .click();
    await expect
      .poll(() => page.evaluate(() => window.browserApprovalFixture.notes))
      .toContain(REMOTE);
    await expect(remote.getByRole("alert")).toBeVisible();

    await page
      .locator('[data-request="device"]')
      .getByRole("button", { name: "Decline", exact: true })
      .click();
    await expect.poll(() => answers.length).toBe(3);
    expect(answers).toEqual([
      { decision: "accept", id: "approve" },
      { decision: "accept", id: "remote" },
      { decision: "decline", id: "device" },
    ]);
    expect(
      await page.evaluate(() => window.browserApprovalFixture.opened),
    ).toEqual([["signIn", browserSignInUrl]]);
  } finally {
    await server.close();
  }
});
