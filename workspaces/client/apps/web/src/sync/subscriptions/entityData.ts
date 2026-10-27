import { syncDatabase, type createSyncClient } from "../client";
import type { DeepReadonly } from "rxdb";
import type { SyncEntityPayload as MutableSyncEntityPayload } from "../../generated/api";
import { SUBSCRIPTION_ENTITY_SCOPES } from "../../generated/subscriptionEntities";
import type { ResourceData, ResourceRef } from "./contracts";
import { selectResourceData, type ResourceDataSelectors } from "./selectData";
import type { ObserveSubscriptionData } from "./module";
import { createWorkDataSelector } from "./workData";

type SyncEntityPayload = DeepReadonly<MutableSyncEntityPayload>;
type ScopeKind = keyof typeof SUBSCRIPTION_ENTITY_SCOPES;
function hasEntityScope(kind: ResourceRef["kind"]): kind is ScopeKind {
  return kind in SUBSCRIPTION_ENTITY_SCOPES;
}

type Selection = {
  entities: readonly SyncEntityPayload[];
  selectWork: ReturnType<typeof createWorkDataSelector>;
};
const selectors: Required<Pick<ResourceDataSelectors<Selection>, ScopeKind>> = {
  work: (ref, { entities, selectWork }) => selectWork(entities, ref.agentId),
  agentDetail: (_ref, { entities }) =>
    entities.find((row) => row.collection === "agentDetail")?.value,
  task: (_ref, { entities }) =>
    entities.find((row) => row.collection === "task")?.value,
  "session-cost": (_ref, { entities }) =>
    entities.find((row) => row.collection === "sessionCost")?.value,
  desktop: (_ref, { entities }) =>
    entities.find((row) => row.collection === "desktop")?.value,
  terminal: (_ref, { entities }) =>
    entities.find((row) => row.collection === "terminal")?.value,
  tasks: (_ref, { entities }) =>
    entities.filter((row) => row.collection === "task").map((row) => row.value),
  monitors: (_ref, { entities }) =>
    entities
      .filter((row) => row.collection === "monitor")
      .map((row) => row.value),
  costs: (_ref, { entities }) =>
    entities
      .filter((row) => row.collection === "accountCost")
      .map((row) => row.value),
  terminals: (_ref, { entities }) =>
    entities
      .filter((row) => row.collection === "terminal")
      .map((row) => row.value),
  state: (_ref, { entities }) => entities,
};

/** Observe only persisted, typed cache rows. Network interest belongs to the
 * subscription module, and each server injects its own projection database. */
export function createEntityDataObserver(
  database: ReturnType<typeof createSyncClient>["syncDatabase"],
): ObserveSubscriptionData {
  return <R extends ResourceRef>(
    ref: R,
    accept: (data: ResourceData<R> | undefined) => void,
    failed: (error: unknown) => void,
  ) => {
    if (!hasEntityScope(ref.kind)) return () => {};
    const scope = SUBSCRIPTION_ENTITY_SCOPES[ref.kind];
    const reference: Readonly<Record<string, unknown>> = ref;
    const selectedAgent = scope.agentField
      ? reference[scope.agentField]
      : undefined;
    const needsTeam = Boolean(selectedAgent && scope.includeTeamMembers);
    const selectedId = scope.idField ? reference[scope.idField] : undefined;
    const collections = needsTeam
      ? [...scope.collections, "agentSummary"]
      : scope.collections;
    const query =
      typeof selectedId === "string" && scope.collections.length === 1
        ? { selector: { id: `entity:${scope.collections[0]}:${selectedId}` } }
        : {
            selector: {
              $or: collections.map((collection) => ({
                id: {
                  $gte: `entity:${collection}:`,
                  $lt: `entity:${collection};`,
                },
              })),
            },
          };
    let stopped = false;
    let stop = () => {};
    let retained = new Map<
      string,
      { seq: number; entity: SyncEntityPayload }
    >();
    let previousEntities: SyncEntityPayload[] | undefined;
    const selectWork = createWorkDataSelector();
    void database()
      .then(({ db }) => {
        if (stopped) return;
        const subscription = db.projections.find(query).$.subscribe({
          next: (rows) => {
            const nextRetained = new Map<
              string,
              { seq: number; entity: SyncEntityPayload }
            >();
            const values = rows.map((row) => row.toJSON());
            const selectedAgents = new Set([selectedAgent]);
            for (const value of needsTeam ? values : []) {
              const entity = value.entity;
              if (
                entity?.collection === "agentSummary" &&
                entity.value.rootId === selectedAgent
              )
                selectedAgents.add(entity.id);
            }
            const entities = values.flatMap((value): SyncEntityPayload[] => {
              const entity = value.entity;
              if (
                value._deleted ||
                !entity ||
                !scope.collections.some(
                  (collection) => collection === entity.collection,
                )
              )
                return [];
              if (scope.idField && entity.id !== reference[scope.idField])
                return [];
              if (
                scope.agentField &&
                reference[scope.agentField] &&
                (!scope.includeTeamMembers ||
                  !("agent" in entity.value) ||
                  !selectedAgents.has(entity.value.agent)) &&
                (!("rootId" in entity.value) ||
                  entity.value.rootId !== selectedAgent)
              )
                return [];
              const prior = retained.get(value.id);
              const selected =
                typeof value.seq === "number" &&
                prior &&
                prior.seq === value.seq
                  ? prior.entity
                  : entity;
              if (typeof value.seq === "number")
                nextRetained.set(value.id, {
                  seq: value.seq,
                  entity: selected,
                });
              return [selected];
            });
            retained = nextRetained;
            if (
              previousEntities &&
              previousEntities.length === entities.length &&
              entities.every(
                (entity, index) => entity === previousEntities![index],
              )
            )
              return;
            previousEntities = entities;
            accept(
              selectResourceData(selectors, ref, { entities, selectWork }),
            );
          },
          error: failed,
        });
        stop = () => subscription.unsubscribe();
      })
      .catch(failed);
    return () => {
      stopped = true;
      stop();
    };
  };
}

export const subscribeEntityData = createEntityDataObserver(() =>
  syncDatabase(),
);
