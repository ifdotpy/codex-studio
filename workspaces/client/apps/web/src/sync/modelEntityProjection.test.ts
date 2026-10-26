import type { SyncEntityPayload } from "../generated/api";
import assert from "node:assert/strict";
import { it } from "vitest";
import { applyEntityRows, emptyEntityProjection } from "./entityProjection.ts";

it("projects per-account model entities, including hidden rows", () => {
  const value = {
    catalogPosition: 0,
    model: "gpt-hidden",
    hidden: true,
    accountKey: "default",
    provider: "codex",
  } satisfies Extract<SyncEntityPayload, { collection: "model" }>["value"];
  const state = emptyEntityProjection();
  applyEntityRows(
    state,
    [
      {
        id: "entity:model:default:gpt-hidden",
        entity: { collection: "model", id: "default:gpt-hidden", value },
        seq: 1,
      },
    ],
    true,
  );

  assert.deepEqual(
    state.values.model.get("default:gpt-hidden"),
    value,
    "the projection retains the complete per-account model row",
  );
});
