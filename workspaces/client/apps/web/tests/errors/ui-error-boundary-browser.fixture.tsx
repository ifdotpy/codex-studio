declare global {
  interface Window {
    uiErrorBoundaryBrowserFixture: {
      setBad: React.Dispatch<React.SetStateAction<boolean>>;
      setScope: React.Dispatch<React.SetStateAction<string>>;
      setFatal: React.Dispatch<React.SetStateAction<boolean>>;
    };
  }
}
import { required } from "../teams/helpers.ts";

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import Boundary from "../../src/components/UIErrorBoundary.tsx";
function Broken({ bad }: { bad: boolean }) {
  if (bad) throw new Error("Fixture display failure");
  return (
    <>
      <p>Feature ready</p>
      <input aria-label="Feature input" defaultValue="Feature value" />
    </>
  );
}
function App() {
  const [bad, setBad] = useState(false),
    [scope, setScope] = useState("one"),
    [fatal, setFatal] = useState(false);
  window.uiErrorBoundaryBrowserFixture = { setBad, setScope, setFatal };
  if (fatal) throw new Error("Fixture root failure");
  return (
    <>
      <nav>
        <button
          onClick={() => {
            setBad(false);
            setScope("two");
          }}
        >
          Other chat
        </button>
      </nav>
      <input aria-label="Draft" defaultValue="Saved draft" />
      <Boundary label="this conversation" resetKey={scope}>
        <Broken bad={bad} />
      </Boundary>
    </>
  );
}
createRoot(required(document.getElementById("root"))).render(
  <Boundary label="Studio" fullPage>
    <App />
  </Boundary>,
);
