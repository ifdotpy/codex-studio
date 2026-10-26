import type {
  FilterKeys,
  OperationRequestBodyContent,
  PathsWithMethod,
} from "openapi-typescript-helpers";
import type { ApiSuccessBodyFor } from "./apiContracts.ts";
import type { components, paths } from "./generated/api.ts";

// Provider and extensible app-server payloads are JSON objects, not untyped maps.
export type JsonValue = components["schemas"]["JsonValue"];
// Input side of generated wire JsonValue: JSON.stringify omits undefined object properties.
export type JsonInput =
  | Exclude<JsonValue, object>
  | JsonInput[]
  | { [key: string]: JsonInput | undefined };
export type Json = Record<string, JsonInput | undefined>;
// Preserve generated field types for renderer values assembled before serialization.
export type JsonInputOf<T> = T extends (infer Item)[]
  ? JsonInputOf<Item>[]
  : T extends object
    ? {
        [Key in keyof T]:
          | JsonInputOf<T[Key]>
          | (string extends Key ? undefined : never);
      }
    : T;

// Transcript records retain typed JSON extension fields on the wire. UI state
// consumes their declared API fields; strip only the generated index signature
// at this local boundary so optional fields keep their narrower types.
export type KnownApiFields<T> = {
  [
    Key in keyof T as string extends Key
      ? never
      : number extends Key
        ? never
        : Key
  ]: T[Key];
};
type TranscriptApiItem = components["schemas"]["TranscriptMessageRecord"];
type TranscriptApiInput = components["schemas"]["TranscriptInput"];
export type TranscriptWireItem = Omit<
  KnownApiFields<TranscriptApiItem>,
  "inputs"
> & {
  inputs?: Array<KnownApiFields<TranscriptApiInput>> | null | undefined;
};
export type TranscriptPageData = Omit<
  components["schemas"]["TranscriptPageResponse"],
  "items"
> & {
  items: TranscriptWireItem[];
};

export type ApiMethod = "get" | "post";

export type ApiPathsFor<M extends ApiMethod> = PathsWithMethod<paths, M> &
  keyof paths;

export type ApiOperation<
  Path extends keyof paths,
  M extends ApiMethod,
> = FilterKeys<paths[Path], M>;

export type ApiGetPath = ApiPathsFor<"get">;

export type ApiPostPath = ApiPathsFor<"post">;

export type GetResult<Path extends ApiPathsFor<"get">> = ApiSuccessBodyFor<
  ApiOperation<Path, "get">
>;

export type PostResult<Path extends ApiPathsFor<"post">> = ApiSuccessBodyFor<
  ApiOperation<Path, "post">
>;

export type PostBody<Path extends ApiPathsFor<"post">> = NonNullable<
  OperationRequestBodyContent<ApiOperation<Path, "post">>
>;
