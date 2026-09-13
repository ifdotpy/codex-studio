import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Json } from "../types";

export function useWorkspaceResource(
  path: string | null,
  revision: string | number,
  cache?: { scope: string; values: Map<string, Json> },
) {
  const key = path && JSON.stringify([cache?.scope || "", path]);
  const values = cache?.values;
  const [state, setState] = useState<{
    key: string | null;
    data: Json | null;
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
        const data = await api(path, undefined, { signal: controller.signal });
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
  }, [path, key, values]);
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
