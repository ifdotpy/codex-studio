import { syncDatabase, type createSyncClient } from "../client";
import type { SyncEntityPayload } from "../../generated/api";
import { SUBSCRIPTION_ENTITY_SCOPES } from "../../generated/subscriptionEntities";
import type { ResourceData, ResourceRef } from "./contracts";
import type { ObserveSubscriptionData } from "./module";

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
    let stopped = false;
    let stop = () => {};
    void database()
      .then(({ db }) => {
        if (stopped) return;
        const subscription = db.projections.find().$.subscribe({
          next: (rows) => {
            const reference = ref as unknown as Record<string, unknown>;
            const selectedAgent = scope.agentField
              ? reference[scope.agentField]
              : undefined;
            const selectedAgents = new Set([selectedAgent]);
            for (const row of rows) {
              const entity = row.toJSON().entity;
              if (
                entity?.collection === "agentSummary" &&
                entity.value.rootId === selectedAgent
              )
                selectedAgents.add(entity.id);
            }
            const entities = rows.flatMap((row): SyncEntityPayload[] => {
              const value = row.toJSON();
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
                  !selectedAgents.has(entity.value.agent))
              )
                return [];
              return [entity];
            });
            let data: unknown;
            switch (ref.kind) {
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
