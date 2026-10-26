/** Default server read deadline, in milliseconds. Network services own the timer. */
export const READ_DEADLINE_MS = 120_000;

/** Maximum API mutation wait, in milliseconds. An expired wait is uncertain. */
export const MUTATION_DEADLINE_MS = 15_000;
