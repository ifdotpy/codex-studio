import { maxPreferenceKeyBytes } from "./preferenceLimits";
type Invoke = <T>(method: string, params: unknown, id?: string) => Promise<T>;
export function scopedPreferenceKey(scope: string, value: unknown) {
  const prefix = `browser:${JSON.stringify(scope)}:`;
  if (
    typeof value !== "string" ||
    !value ||
    new TextEncoder().encode(prefix + value).length > maxPreferenceKeyBytes
  )
    throw new Error("Invalid preference key.");
  return prefix + value;
}

// The caller, never a server frame, selects the namespace.
export function preferenceScope(
  invoke: (method: string, params: unknown, id?: string) => Promise<unknown>,
  scope: string,
): Invoke {
  const prefix = `browser:${JSON.stringify(scope)}:`;
  return async <T>(method: string, input: unknown, id?: string): Promise<T> => {
    const params = input as Record<string, unknown> | null;
    const key = (value: unknown) => scopedPreferenceKey(scope, value);
    if (method === "preferences.raw_all") {
      const all = (await invoke(method, {}, id)) as Record<string, string>;
      return Object.fromEntries(
        Object.entries(all)
          .filter(([name]) => name.startsWith(prefix))
          .map(([name, value]) => [name.slice(prefix.length), value]),
      ) as T;
    }
    if (method === "preferences.import") {
      if (
        !params ||
        params.version !== 1 ||
        !params.entries ||
        typeof params.entries !== "object" ||
        Array.isArray(params.entries)
      )
        throw new Error("Invalid preference snapshot.");
      return (await invoke(
        method,
        {
          version: 1,
          entries: Object.fromEntries(
            Object.entries(params.entries).map(([name, value]) => [
              key(name),
              value,
            ]),
          ),
        },
        id,
      )) as T;
    }
    if (method !== "preferences.raw_set" && method !== "preferences.raw_remove")
      throw new Error("This preference operation is unavailable.");
    return (await invoke(
      method,
      { key: key(params?.key), raw: params?.raw },
      id,
    )) as T;
  };
}
