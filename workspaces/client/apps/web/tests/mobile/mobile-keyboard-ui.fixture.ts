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
  window.scrollTo = ((...args: Parameters<typeof window.scrollTo>) => {
    retainedScroll = 0;
    originalScroll(...args);
  }) as typeof window.scrollTo;
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
  window.setTestViewport = setTestViewport;
  window.retainTestScroll = retainTestScroll;
  window.composerFocusCalls = focusCalls;
  window.composerBlurCount = 0;
}
declare global {
  interface Window {
    setTestViewport: (
      patch: Partial<
        Pick<
          VisualViewport,
          "width" | "height" | "offsetTop" | "offsetLeft" | "scale"
        >
      >,
      type?: string,
    ) => void;
    retainTestScroll: (value: number) => void;
    composerFocusCalls: { focus: number; selection: number };
    composerBlurCount: number;
    composerTextarea: HTMLTextAreaElement;
    composerScrollBefore: {
      page: number;
      document: number;
      transcript: number;
    };
  }
}
