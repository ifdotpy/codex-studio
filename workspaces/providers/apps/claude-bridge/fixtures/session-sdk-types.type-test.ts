import type { SessionFixtureMessage } from "./session-sdk-types.ts";

export const validResult = {
  type: "result",
  subtype: "success",
  is_error: false,
} satisfies SessionFixtureMessage;

// @ts-expect-error Valid fixture results require the SDK lifecycle flag.
export const missingError: SessionFixtureMessage = {
  type: "result",
  subtype: "success",
};

export const unknownResult: SessionFixtureMessage = {
  type: "result",
  // @ts-expect-error Valid fixture results cannot invent an SDK lifecycle subtype.
  subtype: "future_result",
  is_error: false,
};
