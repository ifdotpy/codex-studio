import { test, expect, spawnFixture, readTestState } from "../playwright.mjs";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("visual preferences update across contexts and seed a fresh first render", async ({
  page: runnerPage,
}) => {
  test.setTimeout(180000);
  const root = resolve(import.meta.dirname, "../../../../../../");
  const evidence = await mkdtemp(join(tmpdir(), "studio-preferences-sync-"));
  console.log("Preference evidence:", evidence);
  const proc = spawnFixture(
    "python3",
    [
      "-B",
      join(
        root,
        "workspaces/runtime/apps/server/tests/sidebar-drag-fixture.py",
      ),
      evidence,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  proc.stderr.on("data", (chunk) => {
    log += chunk;
  });
  const contexts = [];
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const url = `http://127.0.0.1:${port}`;
    const browser = runnerPage.context().browser();
    const contextA = await browser.newContext({
      viewport: { width: 1440, height: 1000 },
    });
    const contextB = await browser.newContext({
      viewport: { width: 1440, height: 1000 },
    });
    contexts.push(contextA, contextB);
    const a = await contextA.newPage();
    const b = await contextB.newPage();
    const errors = [];
    for (const page of [a, b])
      page.on("pageerror", (error) => errors.push(error.message));
    await Promise.all([a.goto(url), b.goto(url)]);
    await Promise.all([
      a.locator(".project-tree-toggle").first().waitFor(),
      b.locator(".project-tree-toggle").first().waitFor(),
    ]);
    await a
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await a.getByRole("tab", { name: "Appearance", exact: true }).click();
    await a.getByRole("radio", { name: "Dark", exact: true }).check();
    await expect(b.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await a.getByRole("radio", { name: "Light", exact: true }).check();
    await expect(b.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "light",
    );
    await a
      .getByRole("radiogroup", { name: "Studio text size" })
      .getByRole("radio", { name: "L", exact: true })
      .check();
    await expect
      .poll(() =>
        b.evaluate(() =>
          document.documentElement.style.getPropertyValue(
            "--studio-main-font-size",
          ),
        ),
      )
      .toBe("16px");
    await a.keyboard.press("Escape");
    const projectA = a
      .locator(".project-tree-toggle")
      .filter({ hasText: "Project A" });
    const projectB = b
      .locator(".project-tree-toggle")
      .filter({ hasText: "Project A" });
    await projectA.click();
    await expect(projectB).toHaveAttribute("aria-expanded", "false");
    const initialOrder = await a
      .locator('.project-tree-toggle[data-sidebar-group="projects"]')
      .evaluateAll((rows) =>
        rows.map((row) => row.getAttribute("data-sidebar-id")),
      );
    await projectA.focus();
    await projectA.press("Alt+ArrowDown");
    await expect
      .poll(() =>
        b
          .locator('.project-tree-toggle[data-sidebar-group="projects"]')
          .evaluateAll((rows) =>
            rows.map((row) => row.getAttribute("data-sidebar-id")),
          ),
      )
      .not.toEqual(initialOrder);
    await a
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await a.getByRole("tab", { name: "Servers", exact: true }).click();
    await a
      .getByRole("textbox", { name: "Server alias", exact: true })
      .first()
      .fill("ABC");
    await a
      .getByRole("button", { name: "Save alias", exact: true })
      .first()
      .click();
    await expect
      .poll(() =>
        b.evaluate(() => localStorage.getItem("studio-local-server-alias-v1")),
      )
      .toBe("ABC");
    const contextC = await browser.newContext({
      viewport: { width: 390, height: 844 },
    });
    contexts.push(contextC);
    const c = await contextC.newPage();
    await c.addInitScript(() => {
      window.__firstAppearance = null;
      const observer = new MutationObserver(() => {
        if (
          document.documentElement?.style.getPropertyValue(
            "--studio-main-font-size",
          ) &&
          !window.__firstAppearance
        ) {
          window.__firstAppearance = {
            theme: document.documentElement.dataset.mantineColorScheme,
            size: document.documentElement.style.getPropertyValue(
              "--studio-main-font-size",
            ),
          };
        }
      });
      observer.observe(document, {
        subtree: true,
        attributes: true,
        childList: true,
      });
    });
    await c.goto(url);
    await expect
      .poll(() => c.evaluate(() => window.__firstAppearance))
      .toEqual({ theme: "light", size: "16px" });
    await expect
      .poll(() =>
        c.evaluate(() => localStorage.getItem("studio-local-server-alias-v1")),
      )
      .toBe("ABC");
    await c
      .getByRole("button", { name: "Toggle conversations", exact: true })
      .click();
    await expect(
      c.locator(".project-tree-toggle").filter({ hasText: "Project A" }),
    ).toHaveAttribute("aria-expanded", "false");
    const finalOrder = await b
      .locator('.project-tree-toggle[data-sidebar-group="projects"]')
      .evaluateAll((rows) =>
        rows.map((row) => row.getAttribute("data-sidebar-id")),
      );
    await expect
      .poll(() =>
        c
          .locator('.project-tree-toggle[data-sidebar-group="projects"]')
          .evaluateAll((rows) =>
            rows.map((row) => row.getAttribute("data-sidebar-id")),
          ),
      )
      .toEqual(finalOrder);
    await b.screenshot({
      path: join(evidence, "context-b.png"),
      fullPage: true,
      animations: "disabled",
    });
    await c.screenshot({
      path: join(evidence, "context-c-phone.png"),
      fullPage: true,
      animations: "disabled",
    });
    await writeFile(
      join(evidence, "state.json"),
      JSON.stringify(await readTestState(url), null, 2),
    );
    expect(errors).toEqual([]);
  } finally {
    await Promise.all(contexts.map((context) => context.close()));
    proc.kill("SIGTERM");
    await writeFile(join(evidence, "fixture.log"), log);
  }
});
