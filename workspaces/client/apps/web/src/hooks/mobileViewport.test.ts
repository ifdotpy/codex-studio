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
  focus = vi.fn((_options?: FocusOptions) => {
    documentMock.activeElement = this;
  });
  blur = vi.fn(() => {
    documentMock.activeElement = null;
  });
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
  hasAttribute: (name: string) => boolean;
  style: Map<string, string> & {
    setProperty: (key: string, value: string) => void;
    removeProperty: (key: string) => void;
  };
  setAttribute: ReturnType<
    typeof vi.fn<(name: string, value?: string) => unknown>
  >;
  removeAttribute: ReturnType<typeof vi.fn<(name: string) => unknown>>;
};
let documentMock: EventTarget & {
  documentElement: typeof root;
  activeElement: unknown;
  querySelectorAll: ReturnType<typeof vi.fn>;
  getElementById: ReturnType<typeof vi.fn>;
};
let queries: EventTarget & { matches: boolean };
let frames: Map<number, FrameRequestCallback>;
let cleanup: void | (() => void);

beforeEach(() => {
  viewport = new Viewport();
  queries = Object.assign(new EventTarget(), { matches: true });
  const style = new Map<string, string>();
  const attributes = new Set<string>();
  root = {
    clientHeight: 669,
    hasAttribute: (name) => attributes.has(name),
    style: Object.assign(style, {
      setProperty: (key: string, value: string) => {
        style.set(key, value);
      },
      removeProperty: (key: string) => {
        style.delete(key);
      },
    }),
    setAttribute: vi.fn((name: string) => attributes.add(name)),
    removeAttribute: vi.fn((name: string) => attributes.delete(name)),
  };
  documentMock = Object.assign(new EventTarget(), {
    documentElement: root,
    activeElement: null,
    querySelectorAll: vi.fn(() => []),
    getElementById: vi.fn(() => null),
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
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});
function mount(active = true, compact = false) {
  useMobileViewport(active, compact);
  cleanup = effect.run?.();
}
function flush(time = 0) {
  const pending = [...frames.values()];
  frames.clear();
  pending.forEach((callback) => callback(time));
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
it("pins the Duo keyboard before the viewport event returns", () => {
  mount();
  documentMock.activeElement = new Textarea();
  windowMock.innerHeight = root.clientHeight = 323;
  windowMock.scrollY = 346;
  Object.assign(viewport, { height: 323, offsetTop: 346 });
  viewport.dispatchEvent(new Event("resize"));
  expect(frames.size).toBe(0);
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
  expect(frames.size).toBe(0);
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

it("does not move fixed chrome when document scroll precedes the visual offset", () => {
  mount();
  documentMock.activeElement = new Textarea();
  windowMock.innerHeight = root.clientHeight = 323;
  windowMock.scrollY = 346;
  Object.assign(viewport, { height: 323, pageTop: 346, offsetTop: 0 });
  viewport.dispatchEvent(new Event("resize"));
  expect(root.style.get("--mobile-viewport-top")).toBe("0px");
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
});

it("samples a native focus pan before any viewport event, then stops after blur", () => {
  vi.spyOn(performance, "now").mockReturnValue(0);
  mount();
  documentMock.activeElement = new Textarea();
  documentMock.dispatchEvent(new Event("focusin"));
  for (const [index, offset] of [3, 13, 33, 49, 87, 126, 199, 346].entries()) {
    viewport.offsetTop = offset;
    viewport.height = 323;
    windowMock.innerHeight = 323;
    windowMock.scrollY = offset;
    flush((index + 1) * 16);
    expect(root.style.get("--mobile-viewport-top")).toBe(`${offset}px`);
    expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  }
  documentMock.activeElement = null;
  documentMock.dispatchEvent(new Event("focusout"));
  flush(601);
  flush(602);
  expect(frames.size).toBe(0);
});

it("cancels the native focus sampler when the viewport owner unmounts", () => {
  mount();
  documentMock.activeElement = new Textarea();
  documentMock.dispatchEvent(new Event("focusin"));
  expect(frames.size).toBeGreaterThan(0);
  cleanup?.();
  cleanup = undefined;
  expect(frames.size).toBe(0);
});

it("samples the keyboard close without resize or scroll events", () => {
  vi.spyOn(performance, "now").mockReturnValue(0);
  mount();
  documentMock.activeElement = new Textarea();
  documentMock.dispatchEvent(new Event("focusin"));
  viewport.height = 323;
  viewport.offsetTop = 346;
  windowMock.innerHeight = 323;
  flush(16);
  documentMock.activeElement = null;
  documentMock.dispatchEvent(new Event("focusout"));
  viewport.height = 669;
  viewport.offsetTop = 0;
  windowMock.innerHeight = 669;
  flush(32);
  expect(root.style.get("--mobile-viewport-top")).toBe("0px");
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
});

it("finds the first raw keyboard height loss after a long focus delay", () => {
  const clock = vi.spyOn(performance, "now").mockReturnValue(0);
  mount();
  documentMock.activeElement = new Textarea();
  documentMock.dispatchEvent(new Event("focusin"));
  flush(1000);
  viewport.height = 337;
  windowMock.innerHeight = 669;
  flush(1519);
  expect(root.style.get("--mobile-viewport-height")).toBe("669px");
  expect(root.setAttribute).toHaveBeenCalledWith("data-viewport-keyboard", "");
  expect(frames.size).toBeGreaterThan(0);
  clock.mockReturnValue(1600);
  documentMock.activeElement = null;
  documentMock.dispatchEvent(new Event("focusout"));
  flush(2201);
  flush(2202);
  expect(frames.size).toBe(0);
});

function gesture(target: unknown, type = "pointerdown") {
  const event = new Event(type, { cancelable: true });
  Object.defineProperty(event, "target", { value: target });
  documentMock.dispatchEvent(event);
  return event;
}
it("uses preventScroll for the first composer touch without a debug flag", () => {
  mount();
  const textarea = new Textarea();
  expect(gesture(textarea).defaultPrevented).toBe(true);
  expect(textarea.focus).toHaveBeenCalledWith({ preventScroll: true });
  expect(gesture(textarea, "touchstart").defaultPrevented).toBe(true);
  expect(textarea.focus).toHaveBeenCalledTimes(1);
});
it("refocuses a closed-keyboard textarea but leaves open-keyboard selection native", () => {
  mount();
  const textarea = new Textarea();
  documentMock.activeElement = textarea;
  expect(gesture(textarea).defaultPrevented).toBe(true);
  expect(textarea.blur).toHaveBeenCalledTimes(1);
  expect(textarea.focus).toHaveBeenCalledTimes(1);
  gesture(textarea, "pointerup");
  root.setAttribute("data-viewport-keyboard", "");
  expect(gesture(textarea).defaultPrevented).toBe(false);
  expect(textarea.focus).toHaveBeenCalledTimes(1);
});
it("leaves desktop focus native and removes interception on owner cleanup", () => {
  queries.matches = false;
  mount();
  const textarea = new Textarea();
  expect(gesture(textarea).defaultPrevented).toBe(false);
  queries.matches = true;
  cleanup?.();
  cleanup = undefined;
  expect(gesture(textarea).defaultPrevented).toBe(false);
});
