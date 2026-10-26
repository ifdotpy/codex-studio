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
