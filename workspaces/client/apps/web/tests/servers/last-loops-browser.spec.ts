import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, createFixtureViteServer } from "../playwright.ts";
import { fixture } from "./multi-server-fixture.ts";

for (const outcome of ["applied", "failed"])
  test(`rename ${outcome} uses one POST and saved servers stay idle without status reads`, async ({
    page,
  }) => {
    const state = await mkdtemp(join(tmpdir(), "studio-last-loops-"));
    const receipts = new Map();
    const backend = await fixture("Rename", false, {
      entityFrames: true,
      progress: (resource) =>
        resource.kind === "rename-progress"
          ? { resource, value: receipts.get(resource.requestId) || null }
          : undefined,
    });
    const vite = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../..", import.meta.url)),
      cacheDir: join(state, "vite"),
      server: { host: "127.0.0.1", port: 0, proxy: { "/api": backend.origin } },
    });
    const posts = [],
      statuses = [];
    page.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (path === "/api/rename") posts.push(request.postDataJSON());
      if (path === "/api/multi-server/v1/status") statuses.push(request.url());
    });
    await page.addInitScript(() =>
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([
          {
            id: "unreachable",
            label: "Offline saved server",
            origin: "https://unreachable.tailnet.ts.net",
            credentialId: "missing-credential",
            alias: "OFF",
          },
        ]),
      ),
    );
    await page.route("**/api/rename", (route) => {
      const request = route.request().postDataJSON();
      const receipt = {
        id: request.id,
        request_id: request.request_id,
        status: "pending",
      };
      receipts.set(request.request_id, receipt);
      return route.fulfill({ json: receipt });
    });
    try {
      await vite.listen();
      await page.clock.install();
      await page.goto(`http://127.0.0.1:${vite.httpServer.address().port}`);
      await expect(page.locator("#message")).toBeVisible({ timeout: 20_000 });
      await page.locator("#message").fill("/rename");
      await page
        .getByRole("button", { name: "Send message", exact: true })
        .click();
      await expect.poll(() => posts.length).toBe(1);
      await page.clock.fastForward(35_000);
      expect(posts).toHaveLength(1);
      expect(statuses).toHaveLength(0);
      const requestId = posts[0].request_id;
      receipts.set(requestId, {
        id: "overlap",
        request_id: requestId,
        status: outcome,
        error: outcome === "failed" ? "Stream title failure" : undefined,
      });
      backend.publishRename("overlap", requestId);
      if (outcome === "applied") {
        backend.snapshot.threads[0].name = "Stream renamed chat";
        backend.publishState();
        await expect(
          page
            .getByRole("navigation", { name: "Chats" })
            .getByText("Stream renamed chat", { exact: true }),
        ).toBeVisible();
      } else
        await expect(
          page.getByText("Stream title failure", { exact: true }),
        ).toBeVisible();
      expect(posts).toHaveLength(1);
      expect(statuses).toHaveLength(0);
    } finally {
      await page.close();
      await vite.close();
      await backend.close();
      await rm(state, { recursive: true, force: true });
    }
  });
