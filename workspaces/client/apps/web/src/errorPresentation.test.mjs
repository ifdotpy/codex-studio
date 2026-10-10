import assert from "node:assert/strict";
import { displayError, errorDetails } from "./errorPresentation.ts";
import { nativeErrorView } from "./nativeErrors.ts";

import { it } from "vitest";

it("extracts readable diagnostics and normalizes native error details", () => {
  const diagnostic = {
    message: { error: { message: "Connection refused" } },
    code: 503,
    details: { host: "fixture", retry: false },
  };
  assert.equal(displayError(diagnostic), "Connection refused");
  assert.deepEqual(JSON.parse(errorDetails(diagnostic)), diagnostic);
  for (const value of ["plain error", 0, false, 5]) {
    assert.equal(displayError(value), String(value));
  }
  for (const value of [undefined, null]) assert.equal(displayError(value), "");
  for (const value of [
    { code: "unknown" },
    [{ message: "one" }, { code: "two" }],
  ]) {
    assert.deepEqual(JSON.parse(displayError(value)), value);
  }
  const error = new Error("Local failure", {
    cause: new Error("Original cause"),
  });
  assert.equal(displayError(error), "Local failure");
  assert.equal(JSON.parse(errorDetails(error)).cause.message, "Original cause");
  assert.equal(JSON.parse(errorDetails(error)).stack, error.stack);
  const cyclic = { message: "Cyclic failure" };
  cyclic.self = cyclic;
  assert.equal(displayError(cyclic), "Cyclic failure");
  assert.match(errorDetails(cyclic), /Repeated reference/);
  assert.match(nativeErrorView(cyclic).details, /Cyclic failure/);
  assert.equal(JSON.parse(errorDetails({ code: 3n })).code, "3");
  const invalid = {
    toJSON() {
      throw new Error("Invalid serializer");
    },
  };
  assert.equal(errorDetails(invalid), "Error details could not be displayed.");
  assert.equal(displayError(invalid), "Error details could not be displayed.");
  console.log(
    "PASS: nested diagnostics, complete JSON, primitives, Error causes, cycles, large integers, failed serialization",
  );
});

it("preserves direct detail formatting for nullish, string, and other values", () => {
  assert.equal(errorDetails(null), "");
  assert.equal(errorDetails(undefined), "");
  assert.equal(errorDetails("plain details"), "plain details");
  assert.equal(errorDetails(0), "0");
  assert.equal(errorDetails(false), "false");
  assert.equal(errorDetails(5n), "5");
  assert.equal(errorDetails({ toJSON: () => undefined }), "");
});

it("stringifies primitive nested messages before checking later fields", () => {
  assert.equal(displayError({ message: 42 }), "42");
  assert.equal(displayError({ error: false }), "false");
  assert.equal(
    displayError({ message: 0, error: "Later string message" }),
    "0",
  );
});

it("bounds nested message lookup while preserving fallback diagnostics", () => {
  let deep = { message: "Too deep to summarize" };
  for (let level = 0; level < 8; level++) deep = { message: deep };
  assert.equal(displayError(deep), errorDetails(deep));

  const whitespaceOnly = { message: " \t ", code: "E_EMPTY" };
  assert.equal(displayError(whitespaceOnly), errorDetails(whitespaceOnly));
});

it("breaks repeated message references and continues to a readable fallback", () => {
  let messageReads = 0;
  const loop = {
    get message() {
      messageReads++;
      return this;
    },
    error: "Fallback from the repeated diagnostic",
  };

  assert.equal(displayError(loop), "Fallback from the repeated diagnostic");
  assert.equal(messageReads, 1);
});

it("returns complete diagnostics if service-time display conversion throws", () => {
  const originalToLocaleString = Date.prototype.toLocaleString;
  const diagnostic = {
    message: "You've hit a failure at 2026-01-01T00:00:00Z",
    context: "preserved",
  };
  try {
    Date.prototype.toLocaleString = function () {
      throw new Error("Locale conversion failed");
    };
    assert.equal(displayError(diagnostic), errorDetails(diagnostic));
  } finally {
    Date.prototype.toLocaleString = originalToLocaleString;
  }
});
