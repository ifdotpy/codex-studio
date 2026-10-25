import { syncDatabase, type createSyncClient } from "../client";
import type { SyncEntityPayload } from "../../generated/api";
import { SUBSCRIPTION_ENTITY_SCOPES } from "../../generated/subscriptionEntities";
import type { ResourceData, ResourceRef } from "./contracts";
import type { ObserveSubscriptionData } from "./module";
import { createWorkDataSelector } from "./workData";

type ScopeKind = keyof typeof SUBSCRIPTION_ENTITY_SCOPES;

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
    const scope = SUBSCRIPTION_ENTITY_SCOPES[ref.kind as ScopeKind];
    if (!scope) return () => {};
    const reference = ref as unknown as Record<string, unknown>;
    const selectedAgent = scope.agentField
      ? reference[scope.agentField]
      : undefined;
    const needsTeam = Boolean(selectedAgent && ref.kind !== "work");
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
                !(scope.collections as readonly string[]).includes(
                  entity.collection,
                )
              )
                return [];
              if (scope.idField && entity.id !== reference[scope.idField])
                return [];
              if (
                scope.agentField &&
                reference[scope.agentField] &&
                (!("agent" in entity.value) ||
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
            let data: unknown;
            switch (ref.kind) {
              case "work":
                data = selectWork(entities, ref.agentId);
                break;
              case "agentDetail":
              case "task":
              case "session-cost":
              case "desktop":
              case "terminal":
                data = entities[0]?.value;
                break;
              case "tasks":
              case "monitors":
              case "costs":
              case "terminals":
                data = entities.map((entity) => entity.value);
                break;
              default:
                data = entities;
            }
            accept(data as ResourceData<R> | undefined);
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
