import assert from "node:assert/strict";
import { displayError, errorDetails } from "../web/src/errorPresentation.ts";
import { nativeErrorView } from "../web/src/nativeErrors.ts";

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
