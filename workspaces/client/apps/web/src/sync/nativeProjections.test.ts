import { expect, it, vi } from "vitest";
import { createNativeCoreFixture } from "./nativeCoreFixture";
import { readFileSync } from "node:fs";
import { createNativeProjectionStore, ROW_BATCH } from "./nativeProjections";

const workspace = "a".repeat(32);

async function open(pageRows?: number) {
  const core = createNativeCoreFixture(pageRows);
  const store = createNativeProjectionStore(core.request);
  const projections = await store.open(workspace, "schema1", "");
  return { core, store, projections };
}
const row = (id: string, seq: number, extra: Record<string, unknown> = {}) => ({
  id,
  seq,
  payload: "",
  ...extra,
});
const write = (
  projections: Awaited<ReturnType<typeof open>>["projections"],
  ...rows: ReturnType<typeof row>[]
) =>
  projections.storageInstance.bulkWrite(
    rows.map((document) => ({ document }) as never),
    "test",
  );

it("stores rows in the core and hides tombstones from queries only", async () => {
  const { projections, core } = await open();
  const result = await write(
    projections,
    row("entity:agent:a", 3, { entity: { id: "a" } }),
    row("entity:agent:b", 4, { _deleted: true }),
  );
  expect(result.error).toEqual([]);
  expect(core.calls.map((call) => call.method)).toContain("projection.putRows");
  const found = await projections
    .find({ selector: { id: { $gte: "entity:", $lt: "entity;" } } })
    .exec();
  expect(found.map((doc) => doc.toJSON().id)).toEqual(["entity:agent:a"]);
  const stored = await projections.storageInstance.findDocumentsById(
    ["entity:agent:a", "entity:agent:b", "missing"],
    true,
  );
  expect(stored.map((doc) => [doc.id, doc._deleted])).toEqual([
    ["entity:agent:a", false],
    ["entity:agent:b", true],
  ]);
  expect(
    await projections.storageInstance.findDocumentsById(
      ["entity:agent:b"],
      false,
    ),
  ).toEqual([]);
});

it("pages a range and honours gt, limit, id equality and $or", async () => {
  const { projections } = await open(2);
  await write(
    projections,
    ...["a", "b", "c", "d", "e"].map((id, index) =>
      row(`entity:x:${id}`, index + 1),
    ),
    row("entity:y:a", 9),
    row("other", 10),
  );
  const ids = async (query: Parameters<typeof projections.find>[0]) =>
    (await projections.find(query).exec()).map((doc) => doc.toJSON().id);
  expect(
    await ids({ selector: { id: { $gte: "entity:x:", $lt: "entity:x;" } } }),
  ).toEqual([
    "entity:x:a",
    "entity:x:b",
    "entity:x:c",
    "entity:x:d",
    "entity:x:e",
  ]);
  expect(
    await ids({
      selector: { id: { $gt: "entity:x:b", $lt: "entity:x;" } },
      limit: 2,
    }),
  ).toEqual(["entity:x:c", "entity:x:d"]);
  expect(await ids({ selector: { id: "entity:y:a" } })).toEqual(["entity:y:a"]);
  expect(
    await ids({
      selector: {
        $or: [
          { id: { $gte: "entity:y:", $lt: "entity:y;" } },
          { id: { $gte: "other", $lt: "otheraa" } },
        ],
      },
    }),
  ).toEqual(["entity:y:a", "other"]);
});

it("refuses a write whose base row changed, as RxDB does", async () => {
  const { projections } = await open();
  await write(projections, row("a", 1));
  const [previous] = await projections.storageInstance.findDocumentsById(
    ["a"],
    true,
  );
  const first = await projections.storageInstance.bulkWrite(
    [{ previous: previous!, document: { ...previous!, seq: 2 } } as never],
    "test",
  );
  expect(first.error).toEqual([]);
  const stale = await projections.storageInstance.bulkWrite(
    [{ previous: previous!, document: { ...previous!, seq: 3 } } as never],
    "test",
  );
  expect(stale.error).toEqual([{ status: 409 }]);
  const insert = await write(projections, row("a", 5));
  expect(insert.error).toEqual([{ status: 409 }]);
  const [kept] = await projections.storageInstance.findDocumentsById(
    ["a"],
    true,
  );
  expect(kept?.seq).toBe(2);
});

it("emits query results and change events after writes, and stops on unsubscribe", async () => {
  const { projections } = await open();
  const lists: string[][] = [];
  const changes: string[] = [];
  const one: (string | null)[] = [];
  const query = projections
    .find({ selector: { id: { $gte: "entity:", $lt: "entity;" } } })
    .$.subscribe((docs) => lists.push(docs.map((doc) => doc.id)));
  const feed = projections.$.subscribe((event) =>
    changes.push(event.documentData.id),
  );
  const single = projections
    .findOne("state:entities:ready")
    .$.subscribe((doc) => one.push(doc ? doc.payload : null));
  await vi.waitFor(() => expect(lists).toEqual([[]]));
  await write(
    projections,
    row("entity:a:1", 1),
    row("state:entities:ready", 1, { payload: "ready" }),
  );
  await vi.waitFor(() => expect(lists.at(-1)).toEqual(["entity:a:1"]));
  await vi.waitFor(() => expect(one.at(-1)).toBe("ready"));
  expect(changes).toEqual(["entity:a:1", "state:entities:ready"]);
  query.unsubscribe();
  feed.unsubscribe();
  single.unsubscribe();
  const seen = lists.length;
  await write(projections, row("entity:a:2", 2));
  expect(lists).toHaveLength(seen);
  expect(changes).toHaveLength(2);
});

it("shares one cache per workspace and schema and prunes other schemas", async () => {
  const { store, projections, core } = await open();
  expect(await store.open(workspace, "schema1", "")).toBe(projections);
  await write(projections, row("a", 1));
  const other = await store.open(workspace, "schema2", "");
  expect(await other.storageInstance.findDocumentsById(["a"], true)).toEqual(
    [],
  );
  await store.pruneOtherSchemas(workspace, "schema2", "");
  expect(
    await projections.storageInstance.findDocumentsById(["a"], true),
  ).toEqual([]);
  expect(
    core.calls
      .filter((call) => call.method === "projection.pruneRows")
      .map((call) => call.params.prefix),
  ).toEqual(["schema2"]);
});

it("sends no more rows per request than the core accepts", () => {
  const core = readFileSync(
    new URL(
      "../../../../packages/client-core/src/projections.rs",
      import.meta.url,
    ),
    "utf8",
  );
  expect(core.match(/const MAX_ROW_BATCH: usize = (\d+);/)?.[1]).toBe(
    String(ROW_BATCH),
  );
});
