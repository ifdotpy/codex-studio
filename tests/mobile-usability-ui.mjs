// Headless WebKit audit of the production UI. Only temporary fixture state is used.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { webkit, devices } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const phase =
  process.argv.find((arg) => arg.startsWith("--phase="))?.split("=")[1] ||
  "after";
const verify = phase === "after";
// Screenshots and geometry stay outside the repository.
const evidence = join(
  process.env.STUDIO_EVIDENCE_DIR ||
    join(tmpdir(), "studio-mobile-usability-evidence"),
  phase,
);
await mkdir(evidence, { recursive: true });
const temporary = await mkdtemp(join(tmpdir(), "studio-mobile-usability-"));
const items = [
  {
    id: "audit-user",
    role: "user",
    text: "Review the release and the mobile layouts.",
    turnId: "audit",
    turnStatus: "failed",
  },
  {
    id: "audit-tool",
    role: "output",
    text: JSON.stringify({
      type: "mcpToolCall",
      tool: "check_release",
      status: "failed",
      error:
        "Check failed at /workspace/release/web/src/components/Conversation.tsx:210",
    }),
    turnId: "audit",
    turnStatus: "failed",
    toolStatus: "failed",
  },
  {
    id: "audit-answer",
    role: "assistant",
    text:
      "The release check failed. The report and the command output remain available.\n\n" +
      "This long message checks the transcript width and the actions below the answer. ".repeat(
        6,
      ) +
      "\n\nhttps://example.invalid/" +
      "very-long-path-without-breaks/".repeat(5) +
      "\n\n```tsx\nconst command = 'npm run acceptance -- --reporter=json --output=/workspace/release/reports/mobile-phone-acceptance.json';\nconsole.log(command);\n```\n\n| Component | Result | Next step |\n| :--- | :--- | :--- |\n| Mobile composer | Failed | Read the complete command output |\n| Worker navigation | Passed | Return to the main agent |",
    turnId: "audit",
    turnStatus: "failed",
    turnError:
      "The release check failed. Reconnect to the Mac if the network is unavailable.",
    turnErrorResolved: true,
    phase: "final_answer",
  },
];
await writeFile(join(temporary, "items.json"), JSON.stringify(items));
const fixture = spawn(
  "python3",
  [
    "-B",
    join(repo, "tests/mobile-usability-fixture.py"),
    join(temporary, "Mobile release"),
    join(temporary, "items.json"),
  ],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      MESSAGES_UI_FIXTURE: "1",
      BACKGROUND_UI_FIXTURE: "1",
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(
        ["gpt-6-astra", "gpt-6-luna", "gpt-5.6-luna"].map((model) => ({
          model,
          supportedReasoningEfforts: [{ reasoningEffort: "high" }],
        })),
      ),
    },
  },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
const results = [];
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state")).json();
  const lead = snapshot.threads.find((agent) => agent.name === "Release lead");
  browser = await webkit.launch({ headless: true });
  for (const width of [375, 390, 430, 1440]) {
    const desktop = width === 1440;
    const height = desktop
      ? 960
      : width === 375
        ? 812
        : width === 430
          ? 932
          : 844;
    const context = await browser.newContext(
      desktop
        ? { viewport: { width, height } }
        : {
            ...devices["iPhone 13"],
            viewport: { width, height },
            deviceScaleFactor: 1,
          },
    );
    const page = await context.newPage();
    page.setDefaultTimeout(12000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.addInitScript(
      ({ leadId, width, height, desktop }) => {
        localStorage.setItem(
          desktop ? "codex-opened" : "codex-mobile-opened",
          JSON.stringify(leadId),
        );
        if (desktop) return;
        // WebKit emulation has no software keyboard or installed-app safe area.
        // Use the same visualViewport input contract as mobile-keyboard-ui.mjs.
        const viewport = new EventTarget();
        Object.assign(viewport, {
          width,
          height,
          offsetTop: 0,
          offsetLeft: 0,
          scale: 1,
        });
        Object.defineProperty(window, "visualViewport", {
          configurable: true,
          value: viewport,
        });
        window.auditViewport = (patch) => {
          Object.assign(viewport, patch);
          viewport.dispatchEvent(new Event("resize"));
        };
        window.auditStandalone = () => {
          Object.defineProperty(navigator, "standalone", {
            configurable: true,
            value: true,
          });
          const native = window.matchMedia.bind(window);
          window.matchMedia = (query) =>
            query === "(display-mode: standalone)"
              ? {
                  ...native(query),
                  matches: true,
                  addEventListener() {},
                  removeEventListener() {},
                }
              : native(query);
        };
      },
      { leadId: lead.id, width, height, desktop },
    );
    await page.route("**/api/accounts", (route) =>
      route.fulfill({
        json: {
          accounts: [
            {
              id: "default",
              label: "Personal",
              email: "personal@example.invalid",
              status: "ready",
            },
            {
              id: "work",
              label: "Work",
              email: "mobile-review-account-with-a-long-name@example.invalid",
              status: "ready",
            },
          ],
          defaultAccountKey: "default",
          logins: [],
        },
      }),
    );
    await page.route("**/api/models?*", (route) =>
      route.fulfill({
        json: {
          data: [
            {
              model: "gpt-6-luna",
              displayName: "Luna",
              description: "A model for software work and long tasks.",
              isDefault: true,
              supportedReasoningEfforts: [{ reasoningEffort: "high" }],
            },
            {
              model: "gpt-6-astra",
              displayName: "Astra",
              description:
                "A model with a longer description that must fit on a narrow phone screen.",
              supportedReasoningEfforts: [{ reasoningEffort: "high" }],
            },
          ],
        },
      }),
    );
    // Terminal transport data is a completed fixture shell, never a real PTY.
    await page.route("**/api/terminals", (route) =>
      route.fulfill({
        json: {
          items: [
            {
              id: "audit-terminal",
              agent: lead.id,
              title: "Release logs",
              cwd: "/workspace/release",
              status: "exited",
              exitCode: 0,
              created: 1,
            },
          ],
        },
      }),
    );
    await page.route("**/api/terminals/output?*", (route) =>
      route.fulfill({
        json: {
          text: route.request().url().includes("offset=0")
            ? "Release checks complete\r\n"
            : "",
          offset: 25,
          status: "exited",
          truncated: false,
        },
      }),
    );
    const capture = async (name) => {
      // Mantine drawers need their 200 ms transition to finish before capture.
      await page.waitForTimeout(250);
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      const measurement = await page.evaluate(() => {
        const visible = (node) => {
          const box = node.getBoundingClientRect();
          const css = getComputedStyle(node);
          return (
            box.width > 0 &&
            box.height > 0 &&
            box.bottom > 0 &&
            box.top < innerHeight &&
            css.visibility !== "hidden" &&
            css.display !== "none" &&
            css.opacity !== "0"
          );
        };
        const targets = [
          ...document.querySelectorAll(
            "button, summary, [role=option], [role=tab], [role=switch], input:not([type=hidden]), select",
          ),
        ]
          .filter(visible)
          .map((node) => {
            const box = node.getBoundingClientRect();
            return {
              name:
                node.getAttribute("aria-label") ||
                node.textContent.trim().slice(0, 80),
              selector: node.id || node.className,
              width: Math.round(box.width * 10) / 10,
              height: Math.round(box.height * 10) / 10,
              reachable: node.contains(
                document.elementFromPoint(
                  box.left + box.width / 2,
                  box.top + box.height / 2,
                ),
              ),
            };
          });
        return {
          viewport: { width: innerWidth, height: innerHeight },
          scrollWidth: document.documentElement.scrollWidth,
          root: document
            .querySelector("#root")
            .getBoundingClientRect()
            .toJSON(),
          drawers: [
            ...document.querySelectorAll(".mantine-Drawer-content"),
          ].map((node) => node.getBoundingClientRect().toJSON()),
          header: document
            .querySelector(".workspace-header")
            .getBoundingClientRect()
            .toJSON(),
          targets,
          smallTargets: targets.filter(
            (target) => target.width < 44 || target.height < 44,
          ),
        };
      });
      const path = join(evidence, `${width}-${name}.png`);
      await page.screenshot({ path });
      results.push({
        width,
        name,
        screenshot: relative(repo, path),
        ...measurement,
      });
      console.log(
        `${phase}: ${width} ${name}, ${measurement.smallTargets.length} small targets`,
      );
      if (verify && !desktop) {
        assert.ok(
          measurement.scrollWidth <= width,
          `${name}: page overflow at ${width}`,
        );
        assert.deepEqual(
          measurement.smallTargets,
          [],
          `${name}: targets smaller than 44 px at ${width}`,
        );
      }
    };
    await page.goto(origin);
    await page.locator("#message").waitFor();
    if (desktop)
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .click();
    await page.locator('[data-message$=":audit-answer"]').waitFor();
    await page.locator("#messages").evaluate((node) => {
      node.scrollTop = 0;
    });
    await capture("chat");
    if (verify)
      assert.equal(
        await page.locator(".workspace-header .agent-mode-switch").count(),
        desktop ? 1 : 0,
      );
    await page
      .locator('[data-message$=":audit-answer"] pre')
      .evaluate((node) => node.scrollIntoView({ block: "center" }));
    await capture("long-message-code");
    const work = page.locator(".turn-work > summary").first();
    if (await work.count()) {
      if (
        !(await page.locator(".turn-work").first().getAttribute("open")) &&
        (await page.locator(".turn-work").first().getAttribute("open")) !== ""
      )
        await work.click();
      const detail = page.locator(".tool-card > summary").first();
      if (await detail.count()) await detail.click();
      await work.evaluate((node) => node.scrollIntoView({ block: "start" }));
      await capture("tool-history");
    }
    await page
      .locator("#message")
      .fill("Check the mobile layout.\nKeep this draft on the phone.");
    await capture("composer");
    if (!desktop) {
      await page.evaluate(() =>
        window.auditViewport({ height: 390, offsetTop: 54 }),
      );
      await page.waitForFunction(
        () =>
          Math.abs(
            document.querySelector("#root").getBoundingClientRect().height -
              390,
          ) < 1,
      );
      await capture("keyboard-open");
      if (verify) {
        const box = await page.locator("#send").boundingBox();
        assert.ok(box.y + box.height <= 444, "Send fits above the keyboard");
      }
      await page
        .locator("#message")
        .fill(
          Array.from(
            { length: 12 },
            (_, index) => `Draft line ${index + 1}`,
          ).join("\n"),
        );
      await capture("keyboard-long-draft");
      if (verify) {
        const box = await page.locator("#send").boundingBox();
        assert.ok(
          box.y + box.height <= 444,
          "A long draft keeps Send above the keyboard",
        );
        assert.equal(
          await page
            .locator("#message")
            .evaluate((node) => getComputedStyle(node).overflowY),
          "auto",
        );
      }
      await page
        .locator("#message")
        .fill("Check the mobile layout.\nKeep this draft on the phone.");
      await page.evaluate(
        ({ height }) => window.auditViewport({ height, offsetTop: 0 }),
        { height },
      );
      await page.evaluate(() => window.auditStandalone());
      // Standalone safe-area geometry is simulated explicitly, not claimed as native iOS evidence.
      const standaloneStyle = await page.addStyleTag({
        content:
          ":root { --mobile-safe-area-top: 47px !important; --mobile-safe-area-bottom: 34px !important; }",
      });
      await capture("standalone");
      await standaloneStyle.evaluate((node) => node.remove());
    }
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await page.getByTestId("account-picker").waitFor();
    await capture("settings");
    if (verify && !desktop)
      assert.equal(
        await page.locator(".chat-settings-panel .agent-mode-switch").count(),
        1,
      );
    await page.getByTestId("account-picker").click();
    await capture("account-picker");
    await page.keyboard.press("Escape");
    await page.getByLabel("Main agent settings", { exact: true }).click();
    await capture("model-settings");
    const model = page.locator("#model");
    if (verify) await model.click();
    else await model.evaluate((node) => node.click());
    await capture("model-picker");
    await page.keyboard.press("Escape");
    await page.keyboard.press("Escape");
    await page.keyboard.press("Escape");
    if (!desktop) {
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      await page.evaluate(() =>
        window.auditViewport({ height: 390, offsetTop: 54 }),
      );
      await capture("settings-keyboard");
      await page.keyboard.press("Escape");
      await page.evaluate(
        ({ height }) => window.auditViewport({ height, offsetTop: 0 }),
        { height },
      );
    }
    if (
      (await page
        .getByLabel("Toggle conversations", { exact: true })
        .getAttribute("aria-expanded")) !== "true"
    )
      await page.getByLabel("Toggle conversations", { exact: true }).click();
    await page
      .getByRole("button", { name: "Search chats", exact: true })
      .waitFor();
    await capture("chat-list-project-tree");
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page
      .locator("#conversation-title")
      .filter({ hasText: "Other project" })
      .waitFor();
    await capture("chat-switch");
    if (!desktop)
      await page.getByLabel("Toggle conversations", { exact: true }).click();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    if (desktop)
      await page.getByLabel("Toggle conversations", { exact: true }).click();
    await page.getByLabel("Team", { exact: true }).click();
    await page.locator("#team").waitFor();
    await capture("team-workers");
    await page.locator("#worker-search").fill("Worker 07");
    await capture("worker-error");
    await page.locator("#worker-search").fill("Worker 00");
    await page.locator("#team .worker").first().click();
    await page.locator("#back-lead").waitFor();
    await capture("worker-chat");
    if (verify && !desktop)
      assert.ok(
        await page.locator("#back-lead").evaluate((node) => {
          const box = node.getBoundingClientRect();
          const header = node.closest("header").getBoundingClientRect();
          return box.top >= header.top && box.bottom <= header.bottom;
        }),
        "Back to main agent stays in the phone header",
      );
    await page.locator("#back-lead").click();
    await page.getByRole("button", { name: /^Messages/ }).click();
    await page.locator(".workspace-messages").waitFor();
    await capture("messages");
    await page
      .locator(".workspace-messages .team-room-row")
      .filter({ hasText: "For you" })
      .click();
    await capture("messages-for-you");
    if (!desktop) {
      await page.evaluate(() =>
        window.auditViewport({ height: 390, offsetTop: 54 }),
      );
      await capture("messages-keyboard");
      if (verify) {
        const box = await page
          .locator(".workspace-messages-drawer .mantine-Drawer-content")
          .boundingBox();
        assert.ok(
          box.y >= 54 && box.y + box.height <= 444,
          "Messages drawer fits the keyboard viewport",
        );
      }
      await page.evaluate(
        ({ height }) => window.auditViewport({ height, offsetTop: 0 }),
        { height },
      );
      const standaloneStyle = await page.addStyleTag({
        content:
          ":root { --mobile-safe-area-top: 47px !important; --mobile-safe-area-bottom: 34px !important; }",
      });
      await capture("messages-standalone");
      await standaloneStyle.evaluate((node) => node.remove());
    }
    await page.locator(".mantine-Drawer-close").click();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await capture("chat-actions");
    await page.getByRole("menuitem", { name: /^Current activity/ }).click();
    await page.locator(".tasks-drawer").waitFor();
    await capture("background-tasks");
    await page.locator(".tasks-drawer .task-row").first().click();
    await capture("background-task-details");
    if (!desktop) {
      await page.evaluate(() =>
        window.auditViewport({ height: 390, offsetTop: 54 }),
      );
      await capture("background-keyboard");
      if (verify) {
        const box = await page.locator(".tasks-drawer").boundingBox();
        assert.ok(
          box.y >= 54 && box.y + box.height <= 444,
          "Activity drawer fits the keyboard viewport",
        );
      }
      await page.evaluate(
        ({ height }) => window.auditViewport({ height, offsetTop: 0 }),
        { height },
      );
    }
    await page.locator(".mantine-Drawer-close").click();
    await page
      .getByRole("button", { name: "Chat context", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Advanced analytics", exact: true })
      .click();
    await page.locator(".analytics-root").waitFor();
    await capture("analytics");
    await page.locator(".mantine-Modal-close").click();
    if (desktop) {
      const dock = page.locator(".terminal-dock");
      await dock
        .getByRole("button", { name: "Show terminals", exact: true })
        .click();
      await dock
        .getByRole("button", { name: /Release logs Your terminal/ })
        .click();
      await dock.getByText("Session ended", { exact: true }).waitFor();
      await capture("terminal-output");
      await dock
        .getByRole("button", { name: "Hide terminals", exact: true })
        .click();
    } else {
      assert.equal(
        await page.locator(".terminal-dock").count(),
        0,
        "The current phone UI exposes background commands, not the desktop shell dock",
      );
    }
    let sent;
    await page.route("**/api/messages", (route) => {
      sent = route.request().postDataJSON();
      return route.fulfill({ json: { id: sent.id, status: "pending" } });
    });
    await page
      .locator("#message")
      .fill("Send fixture. Only this temporary server receives the request.");
    await page.locator("#send").click();
    await page.waitForFunction(
      () => document.querySelector(".message-delivery-status")?.textContent,
    );
    await capture("composer-send");
    assert.ok(sent?.id, "Send uses a message identity");
    await context.setOffline(true);
    await page.evaluate(() => window.dispatchEvent(new Event("offline")));
    await capture("offline");
    await context.setOffline(false);
    assert.deepEqual(errors, [], errors.join("\n"));
    await context.close();
  }
  await writeFile(
    join(evidence, "measurements.json"),
    JSON.stringify(results, null, 2) + "\n",
  );
  console.log(
    `mobile-usability-ui: PASS (${phase}); ${results.length} screenshots`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
