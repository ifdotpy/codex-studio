import { phoneSignInUrl } from "./sign-in.fixture.ts";
import type {} from "./sign-in-card-phone-ui.fixture.tsx";
import { createFixtureViteServer } from "../playwright.ts";
import { requestBody } from "../messages/helpers.ts";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
// A sign-in card at phone width (docs/vm-browser-bridge.md, section 6.2). The window has no
// desktop bridge and is not on the server's own Mac, so it opens the link in paste mode,
// then sends the pasted address of the page that did not load once. A device code card
// shows the code with a copy button. The card fits the screen without a horizontal scroll.

import { createServer as createNetServer } from "node:net";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";
import { browserExecutablePath } from "../browser-executable.ts";

const PASTED = "http://127.0.0.1:40645/oauth/callback?code=c1&state=s1";
// A host name that is not a loopback name: the window acts as a phone on the tailnet. The
// browser resolves it to the local Vite server.
const PHONE_HOST = "studio-phone.test";
// A phone reaches Studio over https, a secure context. The test page is plain http, so the
// browser treats its origin as secure; that origin needs its port before the launch.
const PORT = await new Promise<number>((resolve, reject) => {
  const probe = createNetServer();
  probe.once("error", reject);
  probe.listen(0, "127.0.0.1", () => {
    const address = probe.address();
    if (!address || typeof address === "string")
      throw new Error("Missing fixture address");
    const { port } = address;
    probe.close(() => resolve(port));
  });
});
const ORIGIN = `http://${PHONE_HOST}:${PORT}`;

test.use({
  viewport: { width: 390, height: 844 },
  hasTouch: true,
  launchOptions: {
    ...(browserExecutablePath ? { executablePath: browserExecutablePath } : {}),
    args: [
      `--host-resolver-rules=MAP ${PHONE_HOST} 127.0.0.1`,
      `--unsafely-treat-insecure-origin-as-secure=${ORIGIN}`,
    ],
  },
});

test("Sign-in card at phone width uses paste mode and the device code", async ({
  page,
}, testInfo) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const server = await createFixtureViteServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    // Prepare the card dependencies before the first page load.
    optimizeDeps: {
      entries: ["src/components/questions/Requests.tsx"],
      include: ["react", "react-dom/client", "@mantine/core"],
    },
    plugins: [
      {
        name: "sign-in-phone-fixture",
        // The page comes from the same server as its modules, so the browser sees one
        // address space (a routed page would count as public and lose its loopback scripts).
        configureServer(vite) {
          vite.middlewares.use("/check", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module">import "/tests/questions/sign-in-card-phone-ui.fixture.tsx";</script>',
            );
          });
        },
      },
    ],
    server: {
      host: "127.0.0.1",
      port: PORT,
      strictPort: true,
      hmr: false,
      allowedHosts: [PHONE_HOST],
    },
  });
  await server.listen();
  try {
    page.setDefaultTimeout(5000);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("console", (message) => {
      if (message.type() === "error") errors.push(message.text());
    });
    await page.addInitScript((pasted) => {
      window.signInPhoneFixture = {
        opened: [],
        copied: [],
        notes: [],
        waiting: () => {},
      };
      window.open = (url) => {
        window.signInPhoneFixture.opened.push(String(url));
        return window;
      };
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: {
          writeText: async (text: string) =>
            window.signInPhoneFixture.copied.push(text),
          readText: async () => pasted,
        },
      });
    }, PASTED);
    const answers: PostBody<"/api/answer">[] = [];
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "POST")
        return route.fulfill({ json: {} });
      const body = requestBody<PostBody<"/api/answer">>(route);
      answers.push(body);
      if (body.decision === "accept")
        return route.fulfill({
          json: {
            status: "answered",
            openUrl:
              body.id === "redirect" ? phoneSignInUrl : "https://x.invalid/",
          } satisfies PostResult<"/api/answer">,
        });
      return route.fulfill({
        json: { status: "answered" } satisfies PostResult<"/api/answer">,
      });
    });
    // The first load waits for the Vite dependency scan.
    await page.goto(`${ORIGIN}/check`, { timeout: 30_000 });
    expect(await page.evaluate(() => location.hostname)).toBe(PHONE_HOST);
    const redirect = page.locator('[data-request="redirect"]');
    const device = page.locator('[data-request="device"]');
    await redirect.waitFor({ timeout: 30_000 }).catch((error) => {
      throw new Error(`${error.message}\n${errors.join("\n")}`);
    });
    await expect(redirect.getByText("Waiting for you")).toBeVisible();
    await expect(redirect.getByText("Time left")).toBeVisible();
    await expect(redirect.getByText(/^[89] min$/)).toBeVisible();
    await expect(
      redirect.getByText("the chat's server, port 40645"),
    ).toBeVisible();

    const fits = async () =>
      page.evaluate(() => {
        const overflow =
          document.documentElement.scrollWidth -
          document.documentElement.clientWidth;
        const outside = [...document.querySelectorAll("button")].filter(
          (button) => {
            const box = button.getBoundingClientRect();
            return box.left < 0 || box.right > window.innerWidth;
          },
        ).length;
        return { overflow, outside };
      });
    expect(await fits()).toEqual({ overflow: 0, outside: 0 });

    // Device code: the code is large and can be copied; nothing opens.
    await expect(device.getByText("WDJB-MJHT", { exact: true })).toBeVisible();
    await expect(
      device.getByText("Enter it on device.sso.eu-central-1.amazonaws.com"),
    ).toBeVisible();
    await device.getByRole("button", { name: "Copy code" }).click();
    await expect(
      device.getByRole("button", { name: "Code copied" }),
    ).toBeVisible();
    expect(await page.evaluate(() => window.signInPhoneFixture.copied)).toEqual(
      ["WDJB-MJHT"],
    );
    expect(await page.evaluate(() => window.signInPhoneFixture.opened)).toEqual(
      [],
    );
    await page
      .screenshot({
        path: testInfo.outputPath("sign-in-card-phone-approval.png"),
        fullPage: true,
      })
      .catch(() => {});

    // Paste mode: the approval opens the provider page in the system browser.
    await redirect.getByRole("button", { name: "Open in browser" }).click();
    await expect
      .poll(() => page.evaluate(() => window.signInPhoneFixture.opened))
      .toEqual([phoneSignInUrl]);
    expect(answers).toEqual([
      { decision: "accept", paste: true, id: "redirect" },
    ]);

    await page.evaluate(() => window.signInPhoneFixture.waiting());
    await expect(redirect.getByText("Waiting for the browser")).toBeVisible();
    await expect(
      redirect.getByText("Copy its address and paste it here.", {
        exact: false,
      }),
    ).toBeVisible();
    const send = redirect.getByRole("button", { name: "Send to command" });
    await expect(send).toBeDisabled();
    await redirect.getByRole("button", { name: "Paste", exact: true }).click();
    await expect(
      redirect.getByLabel("Address of the page that did not load"),
    ).toHaveValue(PASTED);
    expect(await fits()).toEqual({ overflow: 0, outside: 0 });
    await page
      .screenshot({
        path: testInfo.outputPath("sign-in-card-phone-waiting.png"),
        fullPage: true,
      })
      .catch(() => {});
    await send.click();
    await expect.poll(() => answers.length).toBe(2);
    expect(answers[1]).toEqual({
      decision: "answer",
      result: PASTED,
      id: "redirect",
    });
    await expect
      .poll(() => page.evaluate(() => window.signInPhoneFixture.notes))
      .toContain(
        "Studio sent the sign-in result to the command. The chat shows how it ended.",
      );
    // The field is cleared; a second click cannot send the code again.
    await expect(send).toBeDisabled();
    expect(answers).toHaveLength(2);
    expect(await page.evaluate(() => window.signInPhoneFixture.opened)).toEqual(
      [phoneSignInUrl],
    );
    // The pasted address is not kept in browser storage.
    const stored = await page.evaluate(() =>
      JSON.stringify({ ...localStorage, ...sessionStorage }),
    );
    expect(stored).not.toContain("code=c1");
  } finally {
    await server.close();
  }
});
