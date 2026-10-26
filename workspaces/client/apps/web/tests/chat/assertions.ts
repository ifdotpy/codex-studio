import { expect } from "../playwright.ts";

type Assertions = {
  equal(actual: unknown, expected: unknown, message?: string): void;
  notEqual(actual: unknown, expected: unknown, message?: string): void;
  deepEqual(actual: unknown, expected: unknown, message?: string): void;
  ok(actual: unknown, message?: string): asserts actual;
  match(actual: string, expected: RegExp, message?: string): void;
  doesNotMatch(actual: string, expected: RegExp, message?: string): void;
  fail(message: string): never;
};

// Preserve the Playwright assertion semantics used by the browser scenarios.
export const assert: Assertions = {
  equal: (actual, expected, message) => expect(actual, message).toBe(expected),
  notEqual: (actual, expected, message) =>
    expect(actual, message).not.toBe(expected),
  deepEqual: (actual, expected, message) =>
    expect(actual, message).toEqual(expected),
  ok: (actual, message): asserts actual => {
    expect(actual, message).toBeTruthy();
  },
  match: (actual, expected, message) =>
    expect(actual, message).toMatch(expected),
  doesNotMatch: (actual, expected, message) =>
    expect(actual, message).not.toMatch(expected),
  fail: (message) => {
    throw new Error(message);
  },
};
