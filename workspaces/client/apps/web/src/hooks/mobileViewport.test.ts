import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useMobileViewport } from "./mobileViewport";

const effect = vi.hoisted(() => ({
  run: undefined as undefined | (() => void | (() => void)),
}));
vi.mock("react", () => ({
  useEffect: (callback: () => void | (() => void)) => {
    effect.run = callback;
  },
}));

class Viewport extends EventTarget {
  width = 951;
  height = 669;
  offsetTop = 0;
  scale = 1;
}
class Textarea {
  matches(selector: string) {
    return selector === "#composer textarea";
  }
}
let viewport: Viewport;
let windowMock: EventTarget & {
  visualViewport: Viewport | undefined;
  innerHeight: number;
  scrollX: number;
  scrollY: number;
  matchMedia: ReturnType<typeof vi.fn>;
  scrollTo: ReturnType<typeof vi.fn>;
};
let root: {
  clientHeight: number;
  style: Map<string, string> & {
    setProperty: (key: string, value: string) => void;
    removeProperty: (key: string) => void;
  };
  setAttribute: ReturnType<typeof vi.fn>;
  removeAttribute: ReturnType<typeof vi.fn>;
};
let documentMock: EventTarget & {
  documentElement: typeof root;
  activeElement: unknown;
  querySelectorAll: ReturnType<typeof vi.fn>;
};
let queries: EventTarget & { matches: boolean };
let frames: Map<number, FrameRequestCallback>;
let cleanup: void | (() => void);

beforeEach(() => {
  viewport = new Viewport();
  queries = Object.assign(new EventTarget(), { matches: true });
  const style = new Map<string, string>();
  root = {
    clientHeight: 669,
    style: Object.assign(style, {
      setProperty: (key: string, value: string) => {
        style.set(key, value);
      },
      removeProperty: (key: string) => {
        style.delete(key);
      },
    }),
    setAttribute: vi.fn(),
    removeAttribute: vi.fn(),
  };
  documentMock = Object.assign(new EventTarget(), {
    documentElement: root,
    activeElement: null,
    querySelectorAll: vi.fn(() => []),
  });
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.stubGlobal(
    "MutationObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  windowMock = Object.assign(new EventTarget(), {
    visualViewport: viewport,
    innerHeight: 669,
    scrollX: 0,
    scrollY: 0,
    matchMedia: vi.fn(() => queries),
    scrollTo: vi.fn(),
  });
  frames = new Map();
  vi.stubGlobal("window", windowMock);
  vi.stubGlobal("document", documentMock);
  vi.stubGlobal("navigator", { maxTouchPoints: 0 });
  vi.stubGlobal("HTMLTextAreaElement", Textarea);
  let nextFrame = 0;
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => {
    frames.delete(id);
  });
});
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  vi.unstubAllGlobals();
});
function mount(active = true, compact = false) {
  useMobileViewport(active, compact);
  cleanup = effect.run?.();
}
function flush() {
  const pending = [...frames.values()];
  frames.clear();
  pending.forEach((callback) => callback(0));
}
function resize(patch: Partial<Viewport>, event = "resize") {
  Object.assign(viewport, patch);
  viewport.dispatchEvent(new Event(event));
  flush();
}

it("tracks a wide touch viewport with the keyboard already open", () => {
  viewport.height = 350;
  mount();
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  expect(root.style.get("--mobile-safe-area-bottom")).toBe(
    "env(safe-area-inset-bottom)",
  );
  expect(root.setAttribute).toHaveBeenCalledWith("data-viewport-surface", "");
});
it("pins the Duo keyboard frame when innerHeight shrinks and the document scrolls", () => {
  mount();
  documentMock.activeElement = new Textarea();
  windowMock.innerHeight = root.clientHeight = 323;
  windowMock.scrollY = 346;
  resize({ height: 323, offsetTop: 346 });
  expect(root.style.get("--mobile-viewport-top")).toBe("346px");
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  expect(root.style.get("--mobile-safe-area-bottom")).toBe(
    "env(safe-area-inset-bottom)",
  );
  expect(root.setAttribute).toHaveBeenCalledWith(
    "data-viewport-keyboard-compact",
    "",
  );
  expect(windowMock.scrollTo).not.toHaveBeenCalled();
  windowMock.innerHeight = root.clientHeight = 669;
  windowMock.scrollY = 0;
  resize({ height: 669, offsetTop: 0 });
  expect(root.style.get("--mobile-viewport-top")).toBe("0px");
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  expect(root.style.get("--mobile-safe-area-bottom")).toBe(
    "env(safe-area-inset-bottom)",
  );
});
it("marks short keyboard space below 400px and restores passive status at the boundary", () => {
  viewport.height = 399;
  mount();
  expect(root.setAttribute).toHaveBeenCalledWith(
    "data-viewport-keyboard-compact",
    "",
  );
  root.removeAttribute.mockClear();
  resize({ height: 400 });
  expect(root.removeAttribute).toHaveBeenCalledWith(
    "data-viewport-keyboard-compact",
  );
});
it("does not hide passive status in a short window without keyboard occlusion", () => {
  viewport.height = windowMock.innerHeight = root.clientHeight = 380;
  mount();
  expect(root.setAttribute).not.toHaveBeenCalledWith(
    "data-viewport-keyboard-compact",
    "",
  );
});
it("uses the short touch landscape boundary without changing portrait or tall layouts", () => {
  viewport.height = 699;
  windowMock.innerHeight = root.clientHeight = 699;
  mount();
  expect(root.setAttribute).toHaveBeenCalledWith(
    "data-viewport-short-landscape",
    "",
  );
  windowMock.innerHeight = root.clientHeight = 700;
  resize({ height: 700 });
  expect(root.removeAttribute).toHaveBeenCalledWith(
    "data-viewport-short-landscape",
  );
  root.setAttribute.mockClear();
  resize({ width: 390, height: 669 });
  expect(root.setAttribute).not.toHaveBeenCalledWith(
    "data-viewport-short-landscape",
    "",
  );
});
it("does not use the compact header for a short non-touch document", () => {
  queries.matches = false;
  viewport.height = windowMock.innerHeight = root.clientHeight = 585;
  mount(true, true);
  expect(root.setAttribute).not.toHaveBeenCalledWith(
    "data-viewport-short-landscape",
    "",
  );
});
it("does not constrain an ordinary desktop document", () => {
  queries.matches = false;
  mount();
  expect(root.style.size).toBe(0);
});
it("retains compact support without touch capability", () => {
  queries.matches = false;
  mount(true, true);
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
});
it("tracks touch hardware when a fine pointer is primary", () => {
  queries.matches = false;
  vi.stubGlobal("navigator", { maxTouchPoints: 5 });
  mount();
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
});
it("updates toolbar, keyboard, offset, and rotation geometry without focus changes", () => {
  mount();
  resize({ height: 620 });
  expect(root.style.get("--mobile-safe-area-bottom")).toBe(
    "env(safe-area-inset-bottom)",
  );
  resize({ height: 350, offsetTop: 20 });
  expect(root.style.get("--mobile-viewport-top")).toBe("20px");
  expect(root.style.get("--mobile-safe-area-top")).toBe(
    "max(0px, calc(env(safe-area-inset-top) - 20px))",
  );
  expect(root.style.get("--mobile-safe-area-bottom")).toBe(
    "env(safe-area-inset-bottom)",
  );
  windowMock.innerHeight = root.clientHeight = 951;
  Object.assign(viewport, { height: 951, offsetTop: 0 });
  windowMock.dispatchEvent(new Event("orientationchange"));
  flush();
  expect(root.style.get("--mobile-viewport-height")).toBe("951px");
});
it("keeps pinch zoom under browser control", () => {
  mount();
  resize({ height: 300, offsetTop: 100, scale: 2 });
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  expect(root.style.get("--mobile-viewport-top")).toBe("0px");
});
it("uses the window height without visualViewport", () => {
  windowMock.visualViewport = undefined;
  mount();
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  windowMock.innerHeight = root.clientHeight = 420;
  windowMock.dispatchEvent(new Event("resize"));
  flush();
  expect(root.style.get("--mobile-viewport-height")).toBe("420px");
});
it("preserves the focused composer scroll exception", () => {
  windowMock.scrollY = 20;
  documentMock.activeElement = new Textarea();
  mount();
  expect(windowMock.scrollTo).not.toHaveBeenCalled();
  documentMock.activeElement = null;
  windowMock.dispatchEvent(new Event("scroll"));
  flush();
  expect(windowMock.scrollTo).toHaveBeenCalledWith(0, 0);
});
it("releases geometry and all event listeners on cleanup", () => {
  mount();
  viewport.dispatchEvent(new Event("resize"));
  expect(frames.size).toBe(1);
  cleanup?.();
  cleanup = undefined;
  expect(root.style.size).toBe(0);
  expect(root.removeAttribute).toHaveBeenCalledWith("data-viewport-surface");
  for (const target of [windowMock, viewport, queries]) {
    for (const event of ["resize", "scroll", "orientationchange", "change"])
      target.dispatchEvent(new Event(event));
  }
  expect(frames.size).toBe(0);
});
it("does not observe an inactive App", () => {
  mount(false);
  expect(root.style.size).toBe(0);
});
it("responds when pointer capability changes", () => {
  queries.matches = false;
  mount();
  queries.matches = true;
  queries.dispatchEvent(new Event("change"));
  flush();
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  queries.matches = false;
  queries.dispatchEvent(new Event("change"));
  flush();
  expect(root.style.size).toBe(0);
});

it("re-measures a short post-keyboard webview after focusout and a scroll reset", () => {
  mount();
  documentMock.activeElement = new Textarea();
  windowMock.innerHeight = root.clientHeight = 323;
  resize({ height: 323, offsetTop: 346 });
  documentMock.activeElement = null;
  windowMock.innerHeight = root.clientHeight = 587;
  resize({ height: 587, offsetTop: 0 });
  windowMock.scrollTo.mockImplementation(() => {
    windowMock.innerHeight = root.clientHeight = 669;
    Object.assign(viewport, { height: 669, offsetTop: 0 });
  });
  documentMock.dispatchEvent(new Event("focusout"));
  flush();
  flush();
  expect(windowMock.scrollTo).toHaveBeenCalledWith(0, 0);
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
});

it("keeps the full frame when the active same-document App changes during the keyboard", () => {
  mount();
  documentMock.activeElement = new Textarea();
  windowMock.innerHeight = root.clientHeight = 323;
  resize({ height: 323, offsetTop: 346 });
  cleanup?.();
  cleanup = undefined;
  mount();
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
});

it("keeps the portrait viewport fallback outside short touch landscape", () => {
  viewport.width = 390;
  mount();
  documentMock.activeElement = new Textarea();
  windowMock.innerHeight = root.clientHeight = 323;
  resize({ height: 323, offsetTop: 346 });
  expect(root.style.get("--mobile-viewport-height")).toBe("323px");
  expect(root.style.get("--mobile-safe-area-bottom")).toBe("0px");
  root.setAttribute.mockClear();
  resize({ height: 330 });
  expect(root.setAttribute).not.toHaveBeenCalledWith(
    "data-viewport-short-landscape",
    "",
  );
});
