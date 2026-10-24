import {
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent,
} from "react";
import { SidebarResizeSession } from "./sidebarResize";
import { clampSidebarWidth } from "./sidebarWidth";

export function useSidebarResize(input: {
  scope: string;
  enabled: boolean;
  width: number;
  minimum: number;
  maximum: number;
  step: number;
  reset: () => void;
  commit: (width: number) => void;
}) {
  const session = useRef(new SidebarResizeSession());
  const [draft, setDraft] = useState<number | null>(null);
  const cancel = () => {
    session.current.cancel();
    setDraft(null);
  };
  useLayoutEffect(() => {
    const cancelOnEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") cancel();
    };
    window.addEventListener("keydown", cancelOnEscape, true);
    window.addEventListener("blur", cancel);
    return () => {
      session.current.cancel();
      window.removeEventListener("keydown", cancelOnEscape, true);
      window.removeEventListener("blur", cancel);
    };
  }, []);
  useLayoutEffect(() => {
    cancel();
    return () => session.current.cancel();
  }, [input.scope, input.enabled]);

  const move = (event: PointerEvent<HTMLElement>) => {
    const width = session.current.move(
      input.scope,
      event.pointerId,
      event.clientX,
      input,
    );
    if (width !== null) setDraft(width);
  };
  return {
    width:
      draft === null
        ? input.width
        : clampSidebarWidth(draft, input.minimum, input.maximum),
    handlers: {
      onPointerDown(event: PointerEvent<HTMLElement>) {
        if (!input.enabled || event.button !== 0 || !event.isPrimary) return;
        event.preventDefault();
        event.currentTarget.focus();
        event.currentTarget.setPointerCapture(event.pointerId);
        session.current.start(
          input.scope,
          event.pointerId,
          event.clientX,
          input.width,
        );
      },
      onPointerMove: move,
      onPointerUp(event: PointerEvent<HTMLElement>) {
        const width = session.current.finish(
          input.scope,
          event.pointerId,
          event.clientX,
          input,
        );
        if (width !== null) {
          setDraft(null);
          input.commit(width);
        }
        if (event.currentTarget.hasPointerCapture(event.pointerId))
          event.currentTarget.releasePointerCapture(event.pointerId);
      },
      onPointerCancel: cancel,
      onLostPointerCapture: cancel,
      onDoubleClick: input.reset,
      onKeyDown(event: KeyboardEvent<HTMLElement>) {
        if (event.key === "Escape") {
          cancel();
          return;
        }
        if (["Home", "Enter"].includes(event.key)) {
          event.preventDefault();
          cancel();
          input.reset();
        } else if (["ArrowLeft", "ArrowRight", "End"].includes(event.key)) {
          event.preventDefault();
          cancel();
          input.commit(
            event.key === "End"
              ? input.maximum
              : clampSidebarWidth(
                  input.width +
                    (event.key === "ArrowRight" ? input.step : -input.step),
                  input.minimum,
                  input.maximum,
                ),
          );
        }
      },
    },
  };
}
