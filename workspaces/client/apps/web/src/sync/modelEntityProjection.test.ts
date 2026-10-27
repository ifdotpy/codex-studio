import { ProviderKind } from "../generated/api";
import type { EntityRow } from "./entityProjection.ts";
import type { SyncEntityPayload } from "../generated/api";
import assert from "node:assert/strict";
import { it } from "vitest";
import { applyEntityRows, emptyEntityProjection } from "./entityProjection.ts";

it("projects per-account model entities, including hidden rows", () => {
  // This projection fixture omits the unused catalogPosition.
  const value: Partial<
    Extract<SyncEntityPayload, { collection: "model" }>["value"]
  > = {
    model: "gpt-hidden",
    hidden: true,
    accountKey: "default",
    provider: ProviderKind.codex,
  };
  const state = emptyEntityProjection();
  applyEntityRows(
    state,
    [
      {
        id: "entity:model:default:gpt-hidden",
        payload: "",
        entity: { collection: "model", id: "default:gpt-hidden", value },
        seq: 1,
      } as EntityRow,
    ],
    true,
  );

  assert.deepEqual(
    state.values.model.get("default:gpt-hidden"),
    value,
    "the projection retains the complete per-account model row",
  );
});
