import type { components } from "./api";

type SyncEntity = components["schemas"]["SyncEntity"];

const validEntity: SyncEntity = {
  op: "upsert",
  collection: "task",
  id: "one",
  seq: 1,
  value: { id: "one" },
};

const invalidSequence: SyncEntity = {
  op: "upsert",
  collection: "task",
  id: "one",
  // @ts-expect-error The generated schema requires seq to be numeric.
  seq: "1",
  value: { id: "one" },
};

const invalidField: SyncEntity = {
  op: "upsert",
  collection: "task",
  id: "one",
  seq: 1,
  value: { id: "one" },
  // @ts-expect-error The generated strict model rejects undeclared properties.
  unexpected: true,
};

void validEntity;
void invalidSequence;
void invalidField;

const validDelete: SyncEntity = {
  op: "delete",
  collection: "agent",
  id: "one",
  seq: 2,
};
const invalidValue: SyncEntity = {
  op: "upsert",
  collection: "agent",
  id: "one",
  seq: 1,
  // @ts-expect-error Entity DTO values cannot be encoded JSON strings.
  value: "{}",
};
const invalidDelete: SyncEntity = {
  op: "delete",
  collection: "agent",
  id: "one",
  seq: 2,
  // @ts-expect-error Deletes omit their value.
  value: {},
};
void validDelete;
void invalidValue;
void invalidDelete;
