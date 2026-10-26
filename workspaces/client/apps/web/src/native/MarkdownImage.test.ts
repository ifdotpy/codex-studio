import type { ReactElement } from "react";
import { afterEach, expect, it, vi } from "vitest";

const h = vi.hoisted(() => {
  const session = {
    file: { format: "image", url: "data:image/png;base64,AQ==" },
    release: vi.fn(async () => {}),
  };
  const port = {
    open: vi.fn(async () => session),
    externalImage: vi.fn(async () => session),
  };
  return {
    cursor: 0,
    cells: [] as any[],
    effects: [] as Array<() => void>,
    port,
    owner: Promise.resolve(port),
  };
});
vi.mock("./usePreview", () => ({
  useNativePreview: () => h.owner,
  useNativePreviewPort: () => h.port,
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState(initial: unknown) {
    const i = h.cursor++;
    h.cells[i] ??= {
      value: typeof initial === "function" ? initial() : initial,
    };
    return [
      h.cells[i].value,
      (value: any) => {
        h.cells[i].value =
          typeof value === "function" ? value(h.cells[i].value) : value;
      },
    ];
  },
  useMemo(factory: () => unknown, deps: unknown[]) {
    const i = h.cursor++,
      old = h.cells[i];
    if (!old || deps.some((value, j) => value !== old.deps[j]))
      h.cells[i] = { deps, value: factory() };
    return h.cells[i].value;
  },
  useEffect(effect: () => void | (() => void), deps: unknown[]) {
    const i = h.cursor++,
      old = h.cells[i];
    if (!old || deps.some((value, j) => value !== old.deps[j])) {
      old?.cleanup?.();
      h.cells[i] = { deps };
      h.effects.push(() => {
        h.cells[i].cleanup = effect();
      });
    }
  },
}));
import MarkdownImage from "./MarkdownImage";
function render(src: string) {
  h.cursor = 0;
  return MarkdownImage({ src, alt: "Image" }) as ReactElement<{
    onLoad(): void;
  }>;
}
async function effects() {
  for (const effect of h.effects.splice(0)) effect();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}
afterEach(() => {
  for (const cell of h.cells) cell?.cleanup?.();
  h.cells = [];
  h.effects = [];
  vi.clearAllMocks();
});

it("requires a separate confirmation for each external image address", async () => {
  const first = "https://first.example/image.png",
    second = "https://second.example/image.png";
  render(first);
  await effects();
  expect(h.port.externalImage).not.toHaveBeenCalled();
  render(first).props.onLoad();
  expect(h.port.externalImage).toHaveBeenCalledExactlyOnceWith(first);
  render(first);
  await effects();
  expect(h.port.externalImage).toHaveBeenCalledExactlyOnceWith(first);
  render(second);
  await effects();
  expect(h.port.externalImage).toHaveBeenCalledExactlyOnceWith(first);
  render(second).props.onLoad();
  render(second);
  await effects();
  expect(h.port.externalImage).toHaveBeenLastCalledWith(second);
});
