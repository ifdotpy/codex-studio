import { expect, test } from "vitest";
import {
  isCredentialAction,
  isServerMethod,
  isServerNativeMethod,
} from "./api.ts";

test("desktop transport validators use their closed contract owners", () => {
  for (const method of ["GET", "POST", "HEAD"])
    expect(isServerMethod(method)).toBe(true);
  for (const method of ["DELETE", "get", "toString"])
    expect(isServerMethod(method)).toBe(false);
  expect(isCredentialAction("pairApproved")).toBe(true);
  expect(isCredentialAction("summary")).toBe(true);
  expect(isCredentialAction("invented")).toBe(false);
  expect(isCredentialAction("toString")).toBe(false);
  expect(isServerNativeMethod("openSignIn")).toBe(true);
  expect(isServerNativeMethod("getBackendUpdate")).toBe(true);
  // Local frames expose vault keys directly, never through the server wrapper.
  expect(isServerNativeMethod("showVaultKeys")).toBe(false);
  expect(isServerNativeMethod("notify")).toBe(false);
  expect(isServerNativeMethod("toString")).toBe(false);
});
