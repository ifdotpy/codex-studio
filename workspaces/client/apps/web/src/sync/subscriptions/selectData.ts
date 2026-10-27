import type { ResourceData, ResourceRef } from "./contracts";

type Ref<K extends ResourceRef["kind"]> = Extract<ResourceRef, { kind: K }>;

/** Each selector is checked against the generated resource-to-data owner. */
export type ResourceDataSelectors<Input> = {
  [K in ResourceRef["kind"]]?: (
    ref: Ref<K>,
    input: Input,
  ) => ResourceData<Ref<K>> | undefined;
};

type ResourceRecovery = {
  [K in ResourceRef["kind"]]?: (ref: Ref<K>) => Promise<ResourceData<Ref<K>>>;
};
export type ViewRecovery = Pick<
  ResourceRecovery,
  "processes" | "complaint" | "room" | "terminal-output"
>;

/** TypeScript loses the correlation when indexing a heterogeneous function map.
 * The key and argument come from the same ref; the map checks every result. */
export function selectResourceData<R extends ResourceRef, Input>(
  selectors: ResourceDataSelectors<Input>,
  ref: R,
  input: Input,
): ResourceData<R> | undefined {
  const select = selectors[ref.kind] as
    | ((ref: R, input: Input) => ResourceData<R> | undefined)
    | undefined;
  return select?.(ref, input);
}

export function recoverResourceData<R extends ResourceRef>(
  recovery: ResourceRecovery,
  ref: R,
): Promise<ResourceData<R>> | undefined {
  const recover = recovery[ref.kind] as
    | ((ref: R) => Promise<ResourceData<R>>)
    | undefined;
  return recover?.(ref);
}
