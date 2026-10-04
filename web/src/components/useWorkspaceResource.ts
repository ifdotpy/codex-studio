import { useEffect, useRef, useState } from "react";
import {
  get,
  errorText,
  type ApiGetPath,
  type ApiReadMetadata,
  type GetOptions,
  type GetResult,
} from "../api";

type ResourceOptions<Path extends ApiGetPath> = Pick<
  GetOptions<Path>,
  "query"
> & {
  cache?: { scope: string; values: Map<string, GetResult<Path>> };
};
type ResourceArgs<Path extends ApiGetPath> =
  {} extends Pick<GetOptions<Path>, "query">
    ? [options?: ResourceOptions<Path>]
    : [options: ResourceOptions<Path>];

export function useWorkspaceResource<Path extends ApiGetPath>(
  path: Path | null,
  revision: string | number,
  ...resourceArgs: ResourceArgs<Path>
) {
  const options = resourceArgs[0];
  const query = options?.query;
  const queryKey = JSON.stringify(query || {});
  const cache = options?.cache;
  const key = path && JSON.stringify([cache?.scope || "", path, queryKey]);
  const values = cache?.values;
  const queryRef = useRef(query);
  queryRef.current = query;
  const [state, setState] = useState<{
    key: string | null;
    data: GetResult<Path> | null;
    error: string;
    loading: boolean;
  }>({
    key,
    data: key ? values?.get(key) || null : null,
    error: "",
    loading: !!path && !(key && values?.has(key)),
  });
  const reload = useRef(() => {});
  useEffect(() => {
    let alive = true;
    let etag: string | undefined;
    let busy = false;
    let again = false;
    let controller: AbortController | undefined;
    const load = async () => {
      if (!alive || !path) return;
      if (busy) {
        again = true;
        return;
      }
      busy = true;
      controller = new AbortController();
      const readMetadata: ApiReadMetadata = {};
      setState((old) =>
        old.key === key
          ? { ...old, loading: old.data === null }
          : {
              key,
              data: key ? values?.get(key) || null : null,
              error: "",
              loading: !(key && values?.has(key)),
            },
      );
      try {
        const data = await get(path, {
          query: queryRef.current,
          signal: controller.signal,
          etag,
          readMetadata,
        });
        etag = readMetadata.etag;
        if (readMetadata.notModified || data === undefined) {
          if (alive)
            setState((old) =>
              old.key === key ? { ...old, error: "", loading: false } : old,
            );
          return;
        }
        if (alive) {
          if (values && key) {
            values.delete(key);
            values.set(key, data);
            while (values.size > 16) values.delete(values.keys().next().value!);
          }
          setState({ key, data, error: "", loading: false });
        }
      } catch (error) {
        if (alive)
          setState((old) => ({
            key,
            data: old.key === key ? old.data : null,
            error: errorText(error),
            loading: false,
          }));
      } finally {
        busy = false;
        if (alive && again) {
          again = false;
          void load();
        }
      }
    };
    reload.current = () => {
      void load();
    };
    if (!path) setState({ key, data: null, error: "", loading: false });
    return () => {
      alive = false;
      controller?.abort();
    };
  }, [path, key, queryKey, values]);
  // Refresh requests coalesce. They never invalidate an outstanding response.
  useEffect(() => reload.current(), [path, key, revision]);
  return state.key === key
    ? state
    : {
        key,
        data: key ? values?.get(key) || null : null,
        error: "",
        loading: !!path && !(key && values?.has(key)),
      };
}
