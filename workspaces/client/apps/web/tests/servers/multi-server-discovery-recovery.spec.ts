import { settings } from "./browser-fixture.ts";
import { test, expect } from "../playwright.ts";
import { fixture } from "./multi-server-fixture.ts";

test("another tab cannot clear an uncertain management identity", async ({
  page,
  context,
}) => {
  const local = await fixture("Local");
  const other = await context.newPage();
  try {
    await page.addInitScript(() => {
      const native = window.fetch.bind(window);
      let lost = false;
      window.fetch = async (input, init) => {
        const request = new Request(input, init);
        const action =
          new URL(request.url).pathname === "/api/multi-server" &&
          request.method === "POST"
            ? (await request.clone().json()).action
            : null;
        const response = await native(request);
        if (action === "discover" && !lost) {
          lost = true;
          await response.text();
          throw new TypeError("Discover response lost in fixture");
        }
        return response;
      };
    });
    await Promise.all([page.goto(local.origin), other.goto(local.origin)]);
    const a = await settings(page),
      b = await settings(other);
    await a
      .getByRole("button", { name: "Find servers now", exact: true })
      .click();
    await expect(a.getByRole("alert")).toContainText("Discover response lost");
    await b.getByRole("switch", { name: "Pair servers automatically" }).click();
    await expect(b.getByRole("alert")).toContainText(
      "Retry the saved server request",
    );
    expect(
      local.accessRequests.filter((row) => row.action === "settings"),
    ).toHaveLength(0);
    const pending = await page.evaluate(() =>
      Object.keys(localStorage).filter((key) =>
        key.startsWith("studio-server-management-v1:"),
      ),
    );
    expect(pending).toHaveLength(1);
    const first = local.accessRequests.find((row) => row.action === "discover");
    await a
      .getByRole("button", { name: "Retry saved request", exact: true })
      .click();
    await expect
      .poll(
        () =>
          local.accessRequests.filter((row) => row.action === "discover")
            .length,
      )
      .toBe(2);
    expect(
      local.accessRequests.filter((row) => row.action === "discover"),
    ).toEqual([first, first]);
    await expect
      .poll(() =>
        page.evaluate(
          () =>
            Object.keys(localStorage).filter((key) =>
              key.startsWith("studio-server-management-v1:"),
            ).length,
        ),
      )
      .toBe(0);
  } finally {
    await other.close();
    await local.close();
  }
});
