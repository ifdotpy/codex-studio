export type ApiPathsFor<Paths, Method extends string> = {
  [Path in keyof Paths]: Method extends keyof Paths[Path]
    ? [NonNullable<Paths[Path][Method]>] extends [never]
      ? never
      : Path
    : never;
}[keyof Paths] &
  string;

export type ApiOperationFor<
  Paths,
  Path extends keyof Paths,
  Method extends string,
> = Method extends keyof Paths[Path] ? NonNullable<Paths[Path][Method]> : never;

export type ApiQueryFor<Operation> = Operation extends {
  parameters?: { query?: infer Query };
}
  ? NonNullable<Query>
  : never;

type RequiredKeys<Value> = Value extends object
  ? {
      [Key in keyof Value]-?: {} extends Pick<Value, Key> ? never : Key;
    }[keyof Value]
  : never;

type HasRequiredQuery<Operation> = [ApiQueryFor<Operation>] extends [never]
  ? false
  : [RequiredKeys<ApiQueryFor<Operation>>] extends [never]
    ? false
    : true;

export type ApiPathsWithRequiredQuery<Paths> = {
  [Path in ApiPathsFor<Paths, "get">]: HasRequiredQuery<
    ApiOperationFor<Paths, Path, "get">
  > extends true
    ? Path
    : never;
}[ApiPathsFor<Paths, "get">];

export type ApiRequestBodyFor<Operation> = Operation extends {
  requestBody?: infer RequestBody;
}
  ? NonNullable<RequestBody> extends {
      content: { "application/json": infer Body };
    }
    ? Body
    : never
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
  Paths,
  Options extends {
    timeoutMs?: number;
    workspaceId?: string;
    sessionToken?: string;
    signal?: AbortSignal;
  },
  Metadata,
> = {
  <Path extends ApiPathsFor<Paths, "get">>(
    path: Path,
    options: ApiGetOptions<
      ApiOperationFor<Paths, Path, "get">,
      Options,
      Metadata
    > & { readMetadata: Metadata },
  ): Promise<
    ApiSuccessBodyFor<ApiOperationFor<Paths, Path, "get">> | undefined
  >;
  <Path extends ApiPathsWithRequiredQuery<Paths>>(
    path: Path,
    options: ApiGetOptions<
      ApiOperationFor<Paths, Path, "get">,
      Options,
      Metadata
    > & { readMetadata?: undefined },
  ): Promise<ApiSuccessBodyFor<ApiOperationFor<Paths, Path, "get">>>;
  <
    Path extends Exclude<
      ApiPathsFor<Paths, "get">,
      ApiPathsWithRequiredQuery<Paths>
    >,
  >(
    path: Path,
    options?: ApiGetOptions<
      ApiOperationFor<Paths, Path, "get">,
      Options,
      Metadata
    > & { readMetadata?: undefined },
  ): Promise<ApiSuccessBodyFor<ApiOperationFor<Paths, Path, "get">>>;
};

export type ApiPostContract<
  Paths,
  Options extends {
    timeoutMs?: number;
    workspaceId?: string;
    sessionToken?: string;
    signal?: AbortSignal;
  },
> = <Path extends ApiPathsFor<Paths, "post">>(
  path: Path,
  body: ApiRequestBodyFor<ApiOperationFor<Paths, Path, "post">>,
  options?: Options,
) => Promise<ApiSuccessBodyFor<ApiOperationFor<Paths, Path, "post">>>;

export type ApiSyncGetContract<
  Paths,
  Options extends {
    timeoutMs?: number;
    workspaceId?: string;
    sessionToken?: string;
    signal?: AbortSignal;
  },
  Metadata,
> = ApiGetContract<Paths, Omit<Options, "timeoutMs">, Metadata>;
import type { Readable } from "openapi-typescript-helpers";
