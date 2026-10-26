import { addRxPlugin, createRxDatabase } from "rxdb";
import type {
  ExtractDocumentTypeFromTypedRxJsonSchema,
  RxCollection,
} from "rxdb";
import { getRxStorageDexie } from "rxdb/plugins/storage-dexie";
import { RxDBLeaderElectionPlugin } from "rxdb/plugins/leader-election";

addRxPlugin(RxDBLeaderElectionPlugin);

const schema = {
  version: 0,
  primaryKey: "id",
  type: "object",
  properties: {
    id: { type: "string", maxLength: 300 },
    payload: { type: "string" },
    seq: { type: "integer", minimum: 0, maximum: 9007199254740991 },
  },
  required: ["id", "payload", "seq"],
} as const;

type FixtureDocument = ExtractDocumentTypeFromTypedRxJsonSchema<typeof schema>;
type FixtureCollections = Record<
  "projections" | "drafts" | "outbox",
  RxCollection<FixtureDocument>
>;

/** Opens either the pre-change stable collection set or the new stable set. */
export async function openBuildDatabase(name: string, legacy: boolean) {
  const db = await createRxDatabase<FixtureCollections>({
    name,
    storage: getRxStorageDexie(),
    multiInstance: true,
  });
  await db.addCollections({
    ...(legacy ? { projections: { schema } } : {}),
    drafts: { schema },
    outbox: { schema },
  });
  return db;
}

export async function openProjectionDatabase(name: string) {
  const db = await createRxDatabase<Pick<FixtureCollections, "projections">>({
    name,
    storage: getRxStorageDexie(),
    multiInstance: true,
  });
  await db.addCollections({ projections: { schema } });
  return db;
}

declare global {
  interface Window {
    schemaCacheOldBuildDatabase: Awaited<ReturnType<typeof openBuildDatabase>>;
    schemaCacheNewBuildDatabase: Awaited<ReturnType<typeof openBuildDatabase>>;
  }
}
