import type { components } from "./api";

type SyncEntity = components["schemas"]["SyncEntity"];

const validEntity: SyncEntity = {
  id: "entity:task:one",
  seq: 1,
  payload: "{}",
  _deleted: false,
};

const invalidSequence: SyncEntity = {
  id: "entity:task:one",
  // @ts-expect-error The generated schema requires seq to be numeric.
  seq: "1",
  payload: "{}",
  _deleted: false,
};

const invalidField: SyncEntity = {
  id: "entity:task:one",
  seq: 1,
  payload: "{}",
  _deleted: false,
  // @ts-expect-error The generated strict model rejects undeclared properties.
  unexpected: true,
};

void validEntity;
void invalidSequence;
void invalidField;
