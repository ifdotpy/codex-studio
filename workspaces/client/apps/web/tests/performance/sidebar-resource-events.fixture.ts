// The sidebar fixture measures local projection work without resource transport.
export const watchResourceChanges: typeof import("../../src/sync/resourceEvents.ts").watchResourceChanges =
  () => () => {};
export const watchResourceConnection: typeof import("../../src/sync/resourceEvents.ts").watchResourceConnection =
  () => () => {};
export const acknowledgeEntitySequences: typeof import("../../src/sync/resourceEvents.ts").acknowledgeEntitySequences =
  () => {};
