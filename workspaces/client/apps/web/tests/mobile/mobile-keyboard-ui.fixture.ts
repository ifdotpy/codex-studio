export function installMobileKeyboardUi() {
  const viewport = new EventTarget();
  Object.assign(viewport, {
    width: 390,
    height: 844,
    offsetTop: 0,
    offsetLeft: 0,
    scale: 1,
  });
  Object.defineProperty(window, "visualViewport", {
    configurable: true,
    value: viewport,
  });
  const setTestViewport = (
    patch: Partial<
      Pick<
        VisualViewport,
        "width" | "height" | "offsetTop" | "offsetLeft" | "scale"
      >
    >,
    type = "resize",
  ) => {
    Object.assign(viewport, patch);
    viewport.dispatchEvent(new Event(type));
  };
  let retainedScroll = 0;
  const originalScroll = window.scrollTo.bind(window);
  Object.defineProperty(window, "scrollY", {
    configurable: true,
    get: () => retainedScroll,
  });
  window.scrollTo = (options?: ScrollToOptions | number, y?: number) => {
    retainedScroll = 0;
    if (typeof options === "number") originalScroll(options, y ?? 0);
    else originalScroll(options);
  };
  const retainTestScroll = (value: number) => {
    retainedScroll = value;
    window.dispatchEvent(new Event("scroll"));
  };
  const focusCalls = { focus: 0, selection: 0 };
  const focus = HTMLElement.prototype.focus;
  HTMLElement.prototype.focus = function (...args) {
    focusCalls.focus++;
    return focus.apply(this, args);
  };
  const setSelectionRange = HTMLTextAreaElement.prototype.setSelectionRange;
  HTMLTextAreaElement.prototype.setSelectionRange = function (...args) {
    focusCalls.selection++;
    return setSelectionRange.apply(this, args);
  };
  const harness = {
    setTestViewport,
    retainTestScroll,
    composerFocusCalls: focusCalls,
    composerBlurCount: 0,
    composerTextarea: null as HTMLTextAreaElement | null,
    composerScrollBefore: { page: 0, document: 0, transcript: 0 },
    element(selector: string) {
      const element = document.querySelector(selector);
      if (!(element instanceof HTMLElement))
        throw new Error("Missing fixture element: " + selector);
      return element;
    },
    input() {
      const element = document.querySelector("#message");
      if (!(element instanceof HTMLTextAreaElement))
        throw new Error("Missing fixture textarea");
      return element;
    },
    scrollingElement() {
      const element = document.scrollingElement;
      if (!element) throw new Error("Missing scrolling element");
      return element;
    },
  };
  window.mobileKeyboardUi = harness;
  return harness;
}
declare global {
  interface Window {
    mobileKeyboardUi: ReturnType<typeof installMobileKeyboardUi>;
  }
}
