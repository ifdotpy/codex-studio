import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, createFixtureViteServer } from "../playwright.ts";
import { fixture } from "./multi-server-fixture.ts";

test("hidden server screens release chat details and prefetch while keeping list membership", async ({
  page,
  context,
}) => {
  test.setTimeout(60_000);
  const state = await mkdtemp(join(tmpdir(), "studio-view-subscriptions-"));
  const local = await fixture("Local", false, { resourceBaseline: true });
  const remote = await fixture("Remote", true, { resourceBaseline: true });
  for (const server of [local, remote]) {
    const lead = server.snapshot.threads[0];
    server.setChats([
      lead,
      {
        ...lead,
        id: "prefetch",
        name: "Prefetch chat",
        isLead: false,
        rootId: lead.id,
      },
    ]);
  }
  const vite = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    cacheDir: join(state, "vite"),
    server: { host: "127.0.0.1", port: 0, proxy: { "/api": local.origin } },
  });
  await vite.listen();
  const origin = `http://127.0.0.1:${vite.httpServer.address().port}`;
  const resources = (server) => [...server.subscriptions.values()].flat();
  const has = (server, kind, agentId) =>
    resources(server).some(
      (resource) =>
        resource.kind === kind && (!agentId || resource.agentId === agentId),
    );
  try {
    await context.addInitScript(
      ({ origin, destination }) => {
        const fetchRequest = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (url.origin !== origin) return fetchRequest(request);
          const bytes = await request.clone().arrayBuffer();
          return fetchRequest(destination + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      { origin: remote.invitation.origin, destination: remote.origin },
    );
    await page.goto(origin);
    await expect(page.locator("#message")).toBeVisible();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Servers", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Studio settings" });
    await dialog
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await dialog.getByLabel("Server address").fill(remote.invitation.origin);
    await dialog
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await dialog
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    const chat = (alias) =>
      page.locator("#chat-list .chat-row").filter({
        has: page.locator(`.chat-server-line[aria-label$=", ${alias}"]`),
      });
    await expect(chat("REM")).toContainText("Remote chat");
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await chat("LOC").click();
    for (const kind of ["transcript", "queue", "session-cost"])
      await expect.poll(() => has(local, kind, "overlap")).toBe(true);
    await expect.poll(() => has(local, "transcript", "prefetch")).toBe(true);
    await chat("REM").click();
    for (const kind of ["transcript", "queue", "session-cost", "limits"])
      await expect.poll(() => has(local, kind)).toBe(false);
    await expect.poll(() => has(local, "state")).toBe(true);
    await expect.poll(() => has(local, "drafts")).toBe(true);
    for (const kind of ["transcript", "queue", "session-cost"])
      await expect.poll(() => has(remote, kind, "overlap")).toBe(true);
    await chat("LOC").click();
    await expect.poll(() => has(local, "transcript", "overlap")).toBe(true);
    await expect.poll(() => has(local, "transcript", "prefetch")).toBe(true);
    await expect.poll(() => has(remote, "transcript")).toBe(false);
  } finally {
    await page.close();
    await vite.close();
    await local.close();
    await remote.close();
    await rm(state, { recursive: true, force: true });
  }
});
