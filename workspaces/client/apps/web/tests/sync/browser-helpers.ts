/** Every sync page fixture creates its mount element before loading its module. */
export function fixtureElement(id: string) {
  return document.getElementById(id)!;
}

/** These two recovery fixtures add a marker to their own snapshot documents. */
export type FixtureSnapshot = ReturnType<
  typeof import("../../src/hooks.ts").useSnapshot
> & {
  data:
    | (NonNullable<
        ReturnType<typeof import("../../src/hooks.ts").useSnapshot>["data"]
      > & {
        runtime: { marker?: string };
      })
    | null;
};
