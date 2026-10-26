import React from "react";
import { createRoot } from "react-dom/client";
import { useSnapshot } from "../../src/hooks.ts";

declare global {
  interface Window {
    snapshot: ReturnType<typeof useSnapshot>;
  }
}

const node = document.createElement("div");
document.body.appendChild(node);
function Harness() {
  window.snapshot = useSnapshot();
  return null;
}
createRoot(node).render(React.createElement(Harness));
