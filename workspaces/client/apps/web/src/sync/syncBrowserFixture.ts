import type { ViteDevServer } from "vite";
import type { components } from "../generated/api";

export function fixturePort(server: ViteDevServer): number {
  const address = server.httpServer?.address();
  if (!address || typeof address === "string")
    throw new Error("Fixture server has no TCP address");
  return address.port;
}

export function fixtureSubscriptions(
  body: string,
): components["schemas"]["SyncSubscriptionRequest"] {
  // The renderer and this fixture share the generated subscription request contract.
  return JSON.parse(body) as components["schemas"]["SyncSubscriptionRequest"];
}

export function fixtureResources(
  body: string | null,
): components["schemas"]["ResourceRef"][] {
  // Resource query parameters come from the same renderer's subscription transport.
  return JSON.parse(body || "[]") as components["schemas"]["ResourceRef"][];
}

export type FixturePull = { scope: string | null; after: number };

// These historical fixtures omit generation. Keep their wire bytes while checking
// every field they do send against the generated current response owner.
export function legacyPullResponse(
  value: LegacySyncPullResponse | LegacyEntityPullResponse,
): string {
  return JSON.stringify(value);
}

// The old entity-delivery fixture uses an empty partial value as its tombstone.
// Preserve that legacy wire shape without asserting it is a current upsert DTO.
export type LegacyAgentChange = Omit<
  Extract<
    components["schemas"]["SyncEntity"],
    { collection: "agent"; op: "upsert" }
  >,
  "value"
> & { value: Partial<components["schemas"]["AgentEntityDto"]> };

type LegacyEntityBatch = Omit<
  components["schemas"]["EntityChangeBatch"],
  "documents"
> & {
  documents: LegacyAgentChange[];
};

export type LegacyEntityFrame = Omit<
  components["schemas"]["ResourceChangeEvent"],
  "resourceVersions"
> & {
  resourceVersions: (Omit<
    components["schemas"]["ResourceRevisionEntry"],
    "entityChanges"
  > & {
    entityChanges?: LegacyEntityBatch;
  })[];
};

export type LegacySyncPullResponse =
  | Omit<components["schemas"]["SyncPullResponse"], "generation">
  | Omit<components["schemas"]["SyncPullResetResponse"], "generation">;

type LegacyEntityPullResponse = Omit<
  components["schemas"]["SyncPullResponse"],
  "generation" | "documents"
> & {
  documents: LegacyAgentChange[];
};

// The chat-switch fixture predates protocol 4 and forwards subscription add
// entries unchanged. Keep it distinct from the current resource event contract.
export type LegacySubscriptionFrame = Omit<
  components["schemas"]["ResourceChangeEvent"],
  "protocol" | "resources"
> & {
  protocol: 3;
  resources: (
    | components["schemas"]["ResourceRef"]
    | components["schemas"]["SubscriptionAdd"]
  )[];
};

// Older fixture identities predate the syncProtocol field.
export type LegacySyncIdentityResponse = Pick<
  components["schemas"]["SyncIdentityResponse"],
  "workspaceId"
>;
