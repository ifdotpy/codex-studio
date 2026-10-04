import assert from "node:assert/strict";
import { afterEach, test } from "vitest";

import { copyText } from "./clipboard.ts";

const originalGlobals = new Map(
  ["navigator", "document"].map((name) => [
    name,
    Object.getOwnPropertyDescriptor(globalThis, name),
  ]),
);

function setGlobal(name, value) {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    value,
  });
}

afterEach(() => {
  for (const [name, descriptor] of originalGlobals) {
    if (descriptor) Object.defineProperty(globalThis, name, descriptor);
    else delete globalThis[name];
  }
});

test("copyText awaits the Clipboard API and does not touch the fallback", async () => {
  const calls = [];
  setGlobal("navigator", {
    clipboard: {
      writeText: async (text) => {
        calls.push(text);
      },
    },
  });

  await copyText("secret code");

  assert.deepEqual(calls, ["secret code"]);
});

test("copyText falls back and restores textarea, selection and focus state", async () => {
  const calls = [];
  const ranges = [{ id: "first" }, { id: "second" }];
  const attributes = new Map();
  const styles = {};
  let textarea;
  setGlobal("navigator", {
    clipboard: {
      writeText: async (text) => {
        calls.push(["clipboard", text]);
        throw new Error("permission denied");
      },
    },
  });
  setGlobal("document", {
    activeElement: {
      focus: (options) => calls.push(["focus", options]),
    },
    getSelection: () => ({
      rangeCount: ranges.length,
      getRangeAt: (index) => {
        calls.push(["getRangeAt", index]);
        return ranges[index];
      },
      removeAllRanges: () => calls.push(["clearSelection"]),
      addRange: (range) => calls.push(["restoreRange", range.id]),
    }),
    createElement: (tag) => {
      calls.push(["createElement", tag]);
      textarea = {
        value: "",
        style: styles,
        setAttribute: (name, value) => attributes.set(name, value),
        select: () => calls.push(["select", textarea.value]),
        setSelectionRange: (start, end) =>
          calls.push(["setSelectionRange", start, end]),
        remove: () => calls.push(["remove"]),
      };
      return textarea;
    },
    body: {
      appendChild: (element) => calls.push(["append", element]),
    },
    execCommand: (command) => {
      calls.push(["command", command]);
      return true;
    },
  });

  await copyText("secret code");

  assert.equal(textarea.value, "secret code");
  assert.deepEqual([...attributes], [["readonly", ""]]);
  assert.deepEqual(styles, {
    position: "fixed",
    top: "-1000px",
    opacity: "0",
  });
  assert.deepEqual(
    calls.find(([name]) => name === "createElement"),
    ["createElement", "textarea"],
  );
  assert.deepEqual(
    calls.map(([name]) => name),
    [
      "clipboard",
      "getRangeAt",
      "getRangeAt",
      "createElement",
      "append",
      "select",
      "setSelectionRange",
      "command",
      "remove",
      "clearSelection",
      "restoreRange",
      "restoreRange",
      "focus",
    ],
  );
  assert.deepEqual(
    calls.filter(([name]) => name === "getRangeAt"),
    [
      ["getRangeAt", 0],
      ["getRangeAt", 1],
    ],
  );
  assert.deepEqual(
    calls.find(([name]) => name === "setSelectionRange"),
    ["setSelectionRange", 0, "secret code".length],
  );
  assert.deepEqual(
    calls.find(([name]) => name === "append"),
    ["append", textarea],
  );
  assert.deepEqual(
    calls.find(([name]) => name === "command"),
    ["command", "copy"],
  );
  assert.deepEqual(
    calls.find(([name]) => name === "restoreRange"),
    ["restoreRange", "first"],
  );
  assert.deepEqual(
    calls.find(([name]) => name === "focus"),
    ["focus", { preventScroll: true }],
  );
});

test("copyText cleans up and rejects when the fallback command fails without selection", async () => {
  const calls = [];
  let removed = false;
  setGlobal("navigator", {});
  setGlobal("document", {
    activeElement: null,
    getSelection: () => null,
    createElement: () => ({
      value: "",
      style: {},
      setAttribute: () => {},
      select: () => calls.push("select"),
      setSelectionRange: () => calls.push("setSelectionRange"),
      remove: () => {
        removed = true;
      },
    }),
    body: { appendChild: () => calls.push("append") },
    execCommand: (command) => {
      calls.push(command);
      return false;
    },
  });

  await assert.rejects(copyText("blocked"), /blocked clipboard access/);

  assert.equal(removed, true);
  assert.deepEqual(calls, ["append", "select", "setSelectionRange", "copy"]);
});
