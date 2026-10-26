import { expect, it, vi } from "vitest";
import { createNativeServiceLifecycle } from "./lifecycle";

it("closes every late native view and never delivers into a closed service", async () => {
  let opened!: (close: () => Promise<void>) => void;
  let receive!: (event: { value?: { value: string } }) => void;
  const pending = new Promise<() => Promise<void>>((resolve) => {
    opened = resolve;
  });
  const close = vi.fn(async () => {});
  const accept = vi.fn();
  const scope = createNativeServiceLifecycle(
    {
      async subscribe<T>(
        _topic: string,
        _params: Record<string, unknown>,
        callback: (event: { value?: T }) => void,
      ) {
        receive = (event) => callback({ value: event.value as T });
        return pending;
      },
    },
    vi.fn(),
  );
  scope.watch("preferences", {}, accept);
  let finished = false;
  const disposed = scope.dispose().then(() => {
    finished = true;
  });
  await Promise.resolve();
  expect(finished).toBe(false);
  receive({ value: { value: "late" } });
  expect(accept).not.toHaveBeenCalled();
  opened(close);
  await disposed;
  expect(close).toHaveBeenCalledOnce();
  await scope.dispose();
  expect(close).toHaveBeenCalledOnce();
});
