import { afterEach, expect, it, vi } from "vitest";
import { readEntities } from "../tests/fixtures/rust-ui-fixture.mjs";

afterEach(() => vi.unstubAllGlobals());
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
