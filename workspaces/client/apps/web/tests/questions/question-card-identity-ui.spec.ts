import { fileURLToPath } from "node:url";
import { createFixtureViteServer, test, expect } from "../playwright.ts";
import { requestBody } from "../messages/helpers.ts";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";

test("questions and confirmations edit their original mounted card", async ({
  page,
}, testInfo) => {
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    optimizeDeps: {
      entries: ["tests/questions/question-card-identity-ui.fixture.tsx"],
    },
    plugins: [
      {
        name: "question-identity",
        configureServer(vite) {
          vite.middlewares.use("/check", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<!doctype html><div id="root"></div><script type="module" src="/tests/questions/question-card-identity-ui.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  const answers: PostBody<"/api/answer">[] = [];
  let release: (() => void) | undefined;
  try {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/answer", async (route) => {
      answers.push(requestBody<PostBody<"/api/answer">>(route));
      await new Promise<void>((resolve) => {
        release = resolve;
      });
      await route.fulfill({
        json: { status: "answered" } satisfies PostResult<"/api/answer">,
      });
    });
    const origin = server.resolvedUrls?.local[0];
    if (!origin) throw new Error("Missing fixture origin");
    await page.goto(origin + "check");
    const question = page.locator('[data-request="question"]');
    await expect(question).toHaveCount(1);
    const original = await question.elementHandle();
    await question.getByRole("button", { name: "Answer", exact: true }).click();
    expect(
      await question.evaluate((node, before) => node === before, original),
    ).toBe(true);
    await expect(
      question.getByRole("form", { name: "Reply to the agent" }),
    ).toBeVisible();
    await expect(page.locator(".question-card")).toHaveCount(0);
    await question.getByRole("button", { name: "Yes", exact: true }).click();
    await question.getByRole("button", { name: "Send answer" }).click();
    await expect.poll(() => answers.length).toBe(1);
    expect(
      await question.evaluate((node, before) => node === before, original),
    ).toBe(true);
    expect(answers[0]).toEqual({
      id: "question",
      answers: { "0": { answers: ["Yes"] } },
    });
    release?.();
    await expect(
      page.locator('.question-card[data-question-state="answered"]'),
    ).toHaveCount(1);
    await expect(question).toHaveCount(0);

    const approval = page.locator('[data-request="approval"]');
    const originalApproval = await approval.elementHandle();
    await approval
      .getByRole("button", { name: "Approve", exact: true })
      .click();
    await expect.poll(() => answers.length).toBe(2);
    expect(
      await approval.evaluate(
        (node, before) => node === before,
        originalApproval,
      ),
    ).toBe(true);
    await expect(approval).toHaveCount(1);
    await expect(
      approval.getByRole("button", { name: "Approve", exact: true }),
    ).toBeDisabled();
    expect(answers[1]).toEqual({ id: "approval", decision: "accept" });
    release?.();
    await expect(approval).toHaveCount(0);
    await expect(
      page.locator('.tool-card[data-message="lead:command-item"]'),
    ).toHaveCount(1);
    await page.screenshot({
      path: testInfo.outputPath("answered-question.png"),
    });
    expect(errors).toEqual([]);
  } finally {
    release?.();
    await server.close();
  }
});
