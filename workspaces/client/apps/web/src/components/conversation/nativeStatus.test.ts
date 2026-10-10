import { describe, expect, it } from "vitest";
import { nativeStatusMessage } from "./nativeStatus";

describe("nativeStatusMessage", () => {
  it("falls back to the status message when the error is empty", () => {
    expect(
      nativeStatusMessage({
        error: "",
        message: "The native session needs attention.",
      }),
    ).toBe("The native session needs attention.");
  });

  it("falls back when a structured provider error has no message", () => {
    expect(
      nativeStatusMessage({
        error: { code: "provider_error", message: "" },
        message: "Retrying the native request.",
      }),
    ).toBe("Retrying the native request.");
  });

  it("prefers a useful provider error message", () => {
    expect(
      nativeStatusMessage({
        error: { message: "Provider request failed." },
        message: "Retrying the native request.",
      }),
    ).toBe("Provider request failed.");
  });

  it("ignores closed native status string variants", () => {
    expect(nativeStatusMessage("notLoaded")).toBeUndefined();
  });
});
