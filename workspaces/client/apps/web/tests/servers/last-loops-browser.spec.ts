import assert from "node:assert/strict";
import type { components } from "../../src/generated/api.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import { fixturePostBody } from "./browser-fixture.ts";
import { fixtureOrigin } from "./browser-fixture.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, createFixtureViteServer } from "../playwright.ts";
import { fixture } from "./multi-server-fixture.ts";

for (const outcome of ["applied", "failed"] as const)
  test(`rename ${outcome} uses one POST and saved servers stay idle without status reads`, async ({
    page,
  }) => {
    const state = await mkdtemp(join(tmpdir(), "studio-last-loops-"));
    const receipts = new Map<string, components["schemas"]["RenameResponse"]>();
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
    const posts: PostBody<"/api/rename">[] = [],
      statuses: string[] = [];
    page.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (path === "/api/rename")
        posts.push(fixturePostBody<"/api/rename">(request.postDataJSON()));
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
      const request = fixturePostBody<"/api/rename">(
        route.request().postDataJSON(),
      );
      assert.ok(request.request_id);
      const receipt = {
        id: request.id,
        request_id: request.request_id,
        status: "pending",
      } satisfies components["schemas"]["RenameResponse"];
      receipts.set(request.request_id, receipt);
      return route.fulfill({ json: receipt });
    });
    try {
      await vite.listen();
      await page.clock.install();
      await page.goto(fixtureOrigin(vite));
      await expect(page.locator("#message")).toBeVisible({ timeout: 20_000 });
      await page.locator("#message").fill("/rename");
      await page
        .getByRole("button", { name: "Send message", exact: true })
        .click();
      await expect.poll(() => posts.length).toBe(1);
      await page.clock.fastForward(35_000);
      expect(posts).toHaveLength(1);
      expect(statuses).toHaveLength(0);
      const requestId = posts[0]?.request_id;
      assert.ok(requestId);
      receipts.set(requestId, {
        id: "overlap",
        request_id: requestId,
        status: outcome,
        error: outcome === "failed" ? "Stream title failure" : undefined,
      });
      backend.publishRename("overlap", requestId);
      if (outcome === "applied") {
        assert.ok(backend.snapshot.threads[0]);
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
