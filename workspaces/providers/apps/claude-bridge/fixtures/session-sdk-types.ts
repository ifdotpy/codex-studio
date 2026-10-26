import type { FixtureTextInput } from "../sdk-fixtures.ts";
import type {
  SDKMessage,
  SDKUserMessage,
  Options,
  query,
} from "@anthropic-ai/claude-agent-sdk";

// Scripted SDK doubles supply only fields consumed by their scenario. Preserve
// SDK discriminants and payload types while allowing omitted unrelated fields.
type FixtureValue<T> = T extends readonly (infer V)[]
  ? FixtureValue<V>[]
  : T extends object
    ? { [K in keyof T]?: FixtureValue<T[K]> | undefined }
    : T;
export type SessionFixtureMessage = FixtureValue<SDKMessage>;
export type SessionFixtureQuery = Omit<
  Parameters<typeof query>[0],
  "prompt" | "options"
> & {
  prompt: AsyncGenerator<
    SDKUserMessage & Required<Pick<SDKUserMessage, "uuid" | "session_id">>
  >;
  options: Options & { cwd: string };
};

export type SessionFixtureTextQuery = Omit<SessionFixtureQuery, "prompt"> & {
  prompt: AsyncGenerator<
    FixtureTextInput & Required<Pick<SDKUserMessage, "uuid" | "session_id">>
  >;
};

export type SessionFixtureStreamEvent = NonNullable<
  Extract<SessionFixtureMessage, { type?: "stream_event" | undefined }>["event"]
>;

// Legacy scripted messages use short UUID labels; their SDK payloads stay typed.
export type SessionFixtureLegacyMessage = SessionFixtureMessage extends infer T
  ? T extends unknown
    ? Omit<T, "uuid"> & { uuid?: string }
    : never
  : never;
