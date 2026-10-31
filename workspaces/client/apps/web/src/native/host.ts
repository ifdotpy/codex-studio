// The native host sends subscription payloads in this envelope.
export type NativeEvent<Value = unknown> = { event: string; value: Value };

export type HttpResponse = {
  status: number;
  headers: Record<string, string>;
  base64: string;
};

export type Invoke = (
  command: string,
  args: Record<string, unknown>,
) => Promise<unknown>;
export type Host = {
  core: { invoke: Invoke };
  event?: {
    emit?: (name: string, payload: unknown) => Promise<void>;
    listen: (
      name: string,
      callback: (event: { payload: unknown }) => void,
    ) => Promise<() => void>;
  };
};
export function nativeHost(): Host | undefined {
  return (
    globalThis as typeof globalThis & {
      __TAURI__?: Host;
    }
  ).__TAURI__;
}
