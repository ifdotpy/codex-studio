import {
  createContext,
  useContext,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import "./bottom-inset-band.css";

const BottomBandContext = createContext<HTMLElement | null>(null);

/** Extend backgrounds under Home without extending any control target. */
export function BottomInsetBand({ children }: { children: ReactNode }) {
  const [host] = useState(() => {
    const node = document.createElement("div");
    node.className = "viewport-bottom-band";
    return node;
  });
  const [visible, setVisible] = useState(false);
  useLayoutEffect(() => {
    const root = document.getElementById("root");
    if (!root) return;
    let frame = 0;
    let observedSidebar: Element | undefined;
    let observedWorkspace: Element | undefined;
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    const resize = new ResizeObserver(schedule);
    const update = () => {
      frame = 0;
      const shown =
        document.documentElement.hasAttribute("data-viewport-surface") &&
        Number.parseFloat(getComputedStyle(root).paddingBottom) > 0;
      if (shown && !host.isConnected) document.body.append(host);
      if (!shown) host.remove();
      setVisible(shown);
      const column = [
        ...root.querySelectorAll('#sidebar[data-presentation="inline"]'),
      ].find((node) => node.getBoundingClientRect().width > 0);
      if (column !== observedSidebar) {
        if (observedSidebar) resize.unobserve(observedSidebar);
        if (column) resize.observe(column);
        observedSidebar = column;
      }
      const surface = [...root.querySelectorAll(".workspace")].find(
        (node) => node.getBoundingClientRect().width > 0,
      );
      if (surface !== observedWorkspace) {
        if (observedWorkspace) resize.unobserve(observedWorkspace);
        if (surface) resize.observe(surface);
        observedWorkspace = surface;
      }
      for (const [name, element] of [
        ["workspace", surface],
        ["sidebar", column],
      ] as const) {
        const bounds = element?.getBoundingClientRect();
        host.style.setProperty(`--band-${name}-left`, `${bounds?.left || 0}px`);
        host.style.setProperty(
          `--band-${name}-width`,
          `${bounds?.width || 0}px`,
        );
      }
    };
    resize.observe(root);
    const mutation = new MutationObserver(schedule);
    mutation.observe(document.documentElement, { attributes: true });
    mutation.observe(root, {
      attributes: true,
      childList: true,
      subtree: true,
    });
    window.addEventListener("resize", schedule);
    update();
    return () => {
      cancelAnimationFrame(frame);
      resize.disconnect();
      mutation.disconnect();
      window.removeEventListener("resize", schedule);
      host.remove();
    };
  }, [host]);
  return (
    <BottomBandContext.Provider value={visible ? host : null}>
      {children}
    </BottomBandContext.Provider>
  );
}

/** Only passive session text belongs in this portal. Keep buttons in Usage. */
export function PassiveUsage({ children }: { children: ReactNode }) {
  const host = useContext(BottomBandContext);
  const anchor = useRef<HTMLSpanElement>(null);
  const [rect, setRect] = useState({ left: 0, width: 0 });
  useLayoutEffect(() => {
    const footer = anchor.current?.parentElement;
    if (!host || !footer) return;
    const update = () => {
      const bounds = footer.getBoundingClientRect();
      setRect((old) =>
        old.left === bounds.left && old.width === bounds.width
          ? old
          : { left: bounds.left, width: bounds.width },
      );
    };
    const resize = new ResizeObserver(update);
    resize.observe(footer);
    window.addEventListener("resize", update);
    update();
    return () => {
      resize.disconnect();
      window.removeEventListener("resize", update);
    };
  }, [host]);
  return (
    <span className="usage-passive-anchor" ref={anchor}>
      {host
        ? createPortal(
            <div className="usage-passive-band" style={rect}>
              {children}
            </div>,
            host,
          )
        : children}
    </span>
  );
}
