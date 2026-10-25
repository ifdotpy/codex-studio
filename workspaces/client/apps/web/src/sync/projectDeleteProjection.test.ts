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
  const after = applyEntityChanges(
    state,
    [{ id: "entity:project:a", seq: 14, _deleted: true }],
    true,
  )!;
  expect(after.runtime.projects.map((project) => project.id)).toEqual(["b"]);
  expect(after.runtime.projectSequences).toEqual({ b: 13 });
  expect(applyEntityChanges(state, [project("a", 12)], true)).toBe(after);
});
