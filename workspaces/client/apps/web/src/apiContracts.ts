import type {
  FilterKeys,
  PathsWithMethod,
  Readable,
  RequiredKeysOf,
} from "openapi-typescript-helpers";

export type ApiQueryFor<Operation> = Operation extends {
  parameters?: { query?: infer Query };
}
  ? NonNullable<Query>
  : never;

type IsSuccessStatus<Status> = Status extends number
  ? `${Status}` extends `2${string}`
    ? true
    : false
  : Status extends `2${string}`
    ? true
    : false;

export type ApiSuccessBodyFor<Operation> = Operation extends {
  responses: infer Responses;
}
  ? Readable<
      {
        [Status in keyof Responses]: IsSuccessStatus<Status> extends true
          ? Responses[Status] extends { content: infer Content }
            ? Content extends { "application/json": infer Body }
              ? Body
              : undefined
            : undefined
          : never;
      }[keyof Responses]
    >
  : never;

type HasRequiredQuery<Operation> = [ApiQueryFor<Operation>] extends [never]
  ? false
  : [RequiredKeysOf<ApiQueryFor<Operation>>] extends [never]
    ? false
    : true;

export type ApiPathsWithRequiredQuery<Paths extends {}> = {
  [Path in PathsWithMethod<Paths, "get">]: HasRequiredQuery<
    FilterKeys<Paths[Path], "get">
  > extends true
    ? Path
    : never;
}[PathsWithMethod<Paths, "get">];

export type ApiGetOptions<
  Operation,
  Options extends {
    timeoutMs?: number;
    workspaceId?: string;
    sessionToken?: string;
    signal?: AbortSignal;
  },
  Metadata,
> = Options &
  (HasRequiredQuery<Operation> extends true
    ? { query: ApiQueryFor<Operation> }
    : { query?: ApiQueryFor<Operation> }) &
  (
    | { etag?: never; readMetadata?: never }
    | { etag?: string; readMetadata: Metadata }
  );

export type ApiGetContract<
  Paths extends {},
  Options extends {
    timeoutMs?: number;
    workspaceId?: string;
    sessionToken?: string;
    signal?: AbortSignal;
  },
  Metadata,
> = {
  <Path extends PathsWithMethod<Paths, "get">>(
    path: Path,
    options: ApiGetOptions<
      NonNullable<FilterKeys<Paths[Path], "get">>,
      Options,
      Metadata
    > & { readMetadata: Metadata },
  ): Promise<
    ApiSuccessBodyFor<NonNullable<FilterKeys<Paths[Path], "get">>> | undefined
  >;
  <Path extends ApiPathsWithRequiredQuery<Paths>>(
    path: Path,
    options: ApiGetOptions<
      NonNullable<FilterKeys<Paths[Path], "get">>,
      Options,
      Metadata
    > & { readMetadata?: undefined },
  ): Promise<ApiSuccessBodyFor<NonNullable<FilterKeys<Paths[Path], "get">>>>;
  <
    Path extends Exclude<
      PathsWithMethod<Paths, "get">,
      ApiPathsWithRequiredQuery<Paths>
    >,
  >(
    path: Path,
    options?: ApiGetOptions<
      NonNullable<FilterKeys<Paths[Path], "get">>,
      Options,
      Metadata
    > & { readMetadata?: undefined },
  ): Promise<ApiSuccessBodyFor<NonNullable<FilterKeys<Paths[Path], "get">>>>;
};
