import { MantineProvider } from "@mantine/core";
import mantineCSS from "@mantine/core/styles.css?inline";
import { createStateStore, type StateStore } from "@json-render/core";
import { JSONUIProvider, Renderer } from "@json-render/react";
import { Component, useEffect, useState, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import studioCSS from "../studio-theme.css?inline";
import panelCSS from "./style.css?inline";
import { theme } from "../theme";
import { registry, PanelActions } from "./renderer";
import { panelStates, validatePanelSpec, validateResolved } from "./validate";
import type { PanelCallback } from "../components/PanelDocument";
// Shared geometry implementation used by the native screenshot preflight.
// @ts-expect-error The Electron helper is a plain CommonJS function.
import measureLayout from "../../../desktop/panel-layout.cjs";

declare global {
  interface Window {
    __studioPanelValidate?: () => Promise<{ states: number }>;
    __studioPanelError?: string;
  }
}
const config = JSON.parse(
  decodeURIComponent(
    document.currentScript?.getAttribute("data-config") || "%7B%7D",
  ),
) as {
  spec: unknown;
  callbacks: PanelCallback[];
  channel: string;
  background: string;
};
const send = (type: string, detail: Record<string, unknown> = {}) =>
  parent.postMessage({ type, channel: config.channel, ...detail }, "*");
const fail = (error: unknown) => {
  window.__studioPanelError =
    error instanceof Error ? error.message : String(error);
  send("panel-error", { error: window.__studioPanelError });
};
const style = document.createElement("style");
style.textContent = mantineCSS + studioCSS + panelCSS;
document.head.append(style);
if (config.background)
  document.documentElement.style.setProperty("--surface", config.background);
class Boundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(error: Error) {
    fail(error);
  }
  render() {
    return this.state.failed ? null : this.props.children;
  }
}
const nextFrame = () =>
  new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
try {
  const spec = validatePanelSpec(config.spec, config.callbacks);
  const choices = panelStates(spec, config.callbacks);
  const base = createStateStore(structuredClone(spec.state || {}));
  let checking = false;
  let revision = 0;
  let verified = structuredClone(base.getSnapshot());
  const pendingPaths = new Set<string>();
  const apply = (updates: Record<string, unknown>) => {
    const before = structuredClone(base.getSnapshot());
    const trial = createStateStore(structuredClone(before));
    trial.update(updates);
    try {
      validateResolved(spec, trial.getSnapshot(), config.callbacks);
    } catch (error) {
      send("panel-local-error", {
        error: error instanceof Error ? error.message : String(error),
      });
      return;
    }
    const current = ++revision;
    Object.keys(updates).forEach((path) => pendingPaths.add(path));
    base.update(updates);
    if (!checking)
      requestAnimationFrame(() => {
        if (current !== revision) return;
        const layout = measureLayout();
        if (!layout.fits) {
          // Return to the last state that passed geometry validation. Publishing a new panel still
          // requires the server's complete immutable preflight.
          const restore = Object.fromEntries(
            [...pendingPaths].map((path) => [
              path,
              createStateStore(verified).get(path),
            ]),
          );
          base.update(restore);
          send("panel-local-error", {
            error:
              "This view does not fit the panel. The previous view was restored.",
          });
        } else {
          verified = structuredClone(base.getSnapshot());
          send("panel-local-error", { error: "" });
        }
        pendingPaths.clear();
      });
  };
  const store: StateStore = {
    ...base,
    set: (path, value) => apply({ [path]: value }),
    update: apply,
  };
  let gate = { busy: true, locked: [] as string[] };
  const callbacks = new Map(
    config.callbacks.map((callback) => [callback.id, callback]),
  );
  function View() {
    const [state, setState] = useState(gate);
    useEffect(() => {
      const receive = (event: MessageEvent) => {
        if (
          event.source !== parent ||
          event.data?.channel !== config.channel ||
          event.data?.type !== "panel-state"
        )
          return;
        gate = {
          busy: event.data.busy !== false,
          locked: Array.isArray(event.data.locked) ? event.data.locked : [],
        };
        setState(gate);
      };
      addEventListener("message", receive);
      void document.fonts.ready
        .then(() => nextFrame())
        .then(() => send("panel-ready"));
      return () => removeEventListener("message", receive);
    }, []);
    const callback = (id: string, form?: HTMLFormElement) => {
      if (gate.busy || gate.locked.includes(id)) return;
      const declared = callbacks.get(id);
      if (!declared) return;
      const values: Record<string, string[]> = {};
      if (form) {
        const data = new FormData(form);
        for (const field of declared.fields || []) {
          const entries = data.getAll(field);
          if (entries.some((value) => typeof value !== "string")) return;
          if (entries.length) values[field] = entries as string[];
        }
      }
      gate = { ...gate, busy: true };
      setState(gate);
      send("panel-callback", { callback: id, values });
    };
    return (
      <MantineProvider theme={theme} forceColorScheme="dark">
        <PanelActions.Provider value={{ ...state, send: callback }}>
          <JSONUIProvider registry={registry} store={store}>
            <Renderer spec={spec} registry={registry} />
          </JSONUIProvider>
        </PanelActions.Provider>
      </MantineProvider>
    );
  }
  const root = createRoot(document.getElementById("panel-root")!);
  flushSync(() =>
    root.render(
      <Boundary>
        <View />
      </Boundary>,
    ),
  );
  window.__studioPanelValidate = async () => {
    checking = true;
    const original = structuredClone(base.getSnapshot());
    const replaceState = (snapshot: Record<string, unknown>) =>
      base.update(
        Object.fromEntries(
          Object.entries(snapshot).map(([key, value]) => [
            "/" + key.replaceAll("~", "~0").replaceAll("/", "~1"),
            value,
          ]),
        ),
      );
    try {
      for (const choice of choices) {
        const candidate = createStateStore(structuredClone(spec.state || {}));
        candidate.update(choice);
        flushSync(() => replaceState(candidate.getSnapshot()));
        await nextFrame();
        validateResolved(spec, base.getSnapshot(), config.callbacks);
        if (window.__studioPanelError)
          throw new Error(window.__studioPanelError);
        const layout = measureLayout();
        if (!layout.fits)
          throw new Error(
            `Panel local state ${JSON.stringify(choice)} does not fit at ${innerWidth}x${innerHeight}: ${JSON.stringify(layout.violations.slice(0, 3))}`,
          );
      }
      return { states: choices.length };
    } finally {
      flushSync(() => replaceState(original));
      await nextFrame();
      checking = false;
    }
  };
} catch (error) {
  fail(error);
}
