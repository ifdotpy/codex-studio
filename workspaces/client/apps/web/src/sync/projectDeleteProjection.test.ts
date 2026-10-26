import { expect, it } from "vitest";
import {
  applyEntityChanges,
  emptyEntityProjection,
  type EntityRow,
} from "./entityProjection";

it("retains each project's authoritative sequence for stale-target guards and retires it with SSE tombstones", () => {
  const state = emptyEntityProjection();
  const project = (id: string, seq: number): EntityRow => ({
    id: `entity:project:${id}`,
    seq,
    payload: "",
    entity: {
      collection: "project",
      id,
      value: { id, path: `/${id}`, name: id },
    },
  });
  const baseline = applyEntityChanges(
    state,
    [project("a", 12), project("b", 13)],
    true,
  )!;
  expect(baseline.runtime.projectSequences).toEqual({ a: 12, b: 13 });
  const mixed = applyEntityChanges(
    state,
    [
      {
        id: "entity:room:r",
        seq: 14,
        entity: {
          collection: "room",
          id: "r",
          value: { id: "r", name: "Room" },
        },
      },
    ],
    true,
  )!;
  expect(mixed.runtime.projects).toBe(baseline.runtime.projects);
  expect(mixed.runtime.projectSequences).toBe(
    baseline.runtime.projectSequences,
  );
  const after = applyEntityChanges(
    state,
    [{ id: "entity:project:a", seq: 14, _deleted: true }],
    true,
  )!;
  expect(after.runtime.projects.map((project) => project.id)).toEqual(["b"]);
  expect(after.runtime.projectSequences).toEqual({ b: 13 });
  expect(applyEntityChanges(state, [project("a", 12)], true)).toBe(after);
});
