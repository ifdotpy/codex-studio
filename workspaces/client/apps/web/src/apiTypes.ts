import type { components } from "./generated/api.ts";

// Provider and extensible app-server payloads are JSON objects, not untyped maps.
export type JsonValue = components["schemas"]["JsonValue"];
export type Json = Record<string, JsonValue>;

// Transcript records retain typed JSON extension fields on the wire. UI state
// consumes their declared API fields; strip only the generated index signature
// at this local boundary so optional fields keep their narrower types.
export type KnownApiFields<T> = T extends Record<string, JsonValue> &
  (infer Fields)
  ? Fields
  : T;
type TranscriptApiItem = components["schemas"]["TranscriptMessageRecord"];
type TranscriptApiInput = components["schemas"]["TranscriptInput"];
export type TranscriptWireItem = Omit<
  KnownApiFields<TranscriptApiItem>,
  "inputs"
> & {
  inputs?: Array<KnownApiFields<TranscriptApiInput>> | null;
};
export type TranscriptPageData = Omit<
  components["schemas"]["TranscriptPageResponse"],
  "items"
> & {
  items: TranscriptWireItem[];
};
