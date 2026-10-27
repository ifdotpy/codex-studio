import { SubscriptionReplyKind } from "./generated/api";
import { required } from "./components/componentTestFixtures.ts";
import type { components } from "./generated/api";
import { afterEach, expect, it, vi } from "vitest";
import {
  fixtureProjectDirectory,
  readEntities,
} from "../tests/fixtures/rust-ui-fixture.ts";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readResourceBaselines } from "../tests/fixtures/resource-baselines.ts";

afterEach(() => vi.unstubAllGlobals());
it("uses a disposable project instead of the enclosing checkout", () => {
  const root = mkdtempSync(join(tmpdir(), "studio-fixture-project-"));
  try {
    const project = fixtureProjectDirectory(root);
    expect(project).toBe(join(root, "project"));
    expect(
      execFileSync("git", ["-C", project, "rev-parse", "--verify", "HEAD"], {
        encoding: "utf8",
      }).trim(),
    ).toMatch(/^[0-9a-f]{40,64}$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
it("refuses a project inside the enclosing checkout", () => {
  expect(() => fixtureProjectDirectory(process.cwd())).toThrow(
    /outside the enclosing checkout/,
  );
});
it("reads scoped fixture baselines from fragmented SSE and closes the stream", async () => {
  const resource = {
    kind: "agentDetail",
    agentId: "lead",
  } satisfies components["schemas"]["ResourceRef"];
  const entry = {
    resource: { agentId: "lead", kind: "agentDetail" },
    revision: 3,
    cursor: { workspaceId: "fixture", seq: 3 },
    replyKind: SubscriptionReplyKind.snapshot,
    entityChanges: { after: 0, through: 3, documents: [] },
  } satisfies components["schemas"]["ResourceRevisionEntry"];
  const cancel = vi.fn();
  const frame = `event: resources\ndata: ${JSON.stringify({ resources: [resource], resourceVersions: [entry] })}\n\n`;
  const stream = new ReadableStream({
    start(controller) {
      for (const chunk of [
        "event: session\ndata: {}\n\n",
        frame.slice(0, 50),
        frame.slice(50),
      ])
        controller.enqueue(new TextEncoder().encode(chunk));
    },
    cancel,
  });
  const fetch = vi
    .fn<(url: string) => Promise<Response>>()
    .mockResolvedValue(new Response(stream));
  vi.stubGlobal("fetch", fetch);
  expect(
    await readResourceBaselines("http://fixture.test", [resource]),
  ).toEqual([entry]);

  expect(
    JSON.parse(
      new URL(required(fetch.mock.calls[0])[0]).searchParams.get("resources")!,
    ),
  ).toEqual([resource]);
  expect(cancel).toHaveBeenCalledOnce();
});
it("fixture recovery retries a projection race and consumes an explicit reset", async () => {
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(
      Response.json({ error: "projection changed" }, { status: 409 }),
    )
    .mockResolvedValueOnce(Response.json({ reset: true }))
    .mockResolvedValueOnce(
      Response.json({
        documents: [
          { op: "upsert", collection: "agent", value: { id: "agent" } },
        ],
        checkpoint: { seq: 3 },
        maxSeq: 3,
      }),
    );
  vi.stubGlobal("fetch", fetch);
  expect(await readEntities("http://fixture.test", "agent")).toEqual([
    { id: "agent" },
  ]);
  expect(fetch).toHaveBeenCalledTimes(3);
});
it("fixture recovery preserves a permanent read error", async () => {
  vi.stubGlobal(
    "fetch",
    vi
      .fn()
      .mockResolvedValue(
        Response.json({ error: "unavailable" }, { status: 503 }),
      ),
  );
  await expect(readEntities("http://fixture.test", "agent")).rejects.toThrow(
    "503",
  );
});
