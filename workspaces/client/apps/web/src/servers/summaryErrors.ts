/** The server runs another Studio version: it answers, but its API differs from this one. */
export class SchemaMismatchError extends Error {
  constructor() {
    super("The server summary requires a Studio update.");
  }
}

/** The connection state that a failed summary request shows. */
export const summaryFailure = (error: unknown) =>
  error instanceof SchemaMismatchError
    ? ("schema-mismatch" as const)
    : ("offline" as const);
