import { expect } from "../playwright.ts";

/** Assertion spelling used by the migrated browser scenarios. */
export const assert = {
  equal: (actual: unknown, expected: unknown, message?: string) =>
    expect(actual, message).toBe(expected),
  notEqual: (actual: unknown, expected: unknown, message?: string) =>
    expect(actual, message).not.toBe(expected),
  deepEqual: (actual: unknown, expected: unknown, message?: string) =>
    expect(actual, message).toEqual(expected),
  ok: (actual: unknown, message?: string) =>
    expect(actual, message).toBeTruthy(),
  match: (actual: unknown, expected: RegExp, message?: string) =>
    expect(actual, message).toMatch(expected),
  doesNotMatch: (actual: string, expected: RegExp, message?: string) =>
    expect(actual, message).not.toMatch(expected),
  fail: (message?: string): never => {
    throw new Error(message);
  },
};
